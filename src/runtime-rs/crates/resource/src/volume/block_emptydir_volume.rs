// Copyright (c) 2026 NVIDIA Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::Volume;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use hypervisor::{
    device::{
        device_manager::{do_handle_device, get_block_device_info, DeviceManager},
        DeviceConfig,
    },
    BlockConfigModern, BlockDeviceAio,
};
use kata_sys_util::k8s::is_host_empty_dir;
use kata_types::config::{EMPTYDIR_MODE_BLOCK_ENCRYPTED, EMPTYDIR_MODE_BLOCK_PLAIN};
use kata_types::mount::{
    add_volume_mount_info, is_volume_mounted, DirectVolumeMountInfo,
    DEFAULT_KATA_GUEST_SANDBOX_DIR, KATA_BLOCK_VOLUME_FS_TYPE,
};
use nix::sys::statfs::statfs;
use oci_spec::runtime as oci;
use tokio::sync::RwLock;

use crate::volume::utils::KATA_MOUNT_BIND_TYPE;

const DISK_IMG: &str = "disk.img";
const ENCRYPTION_KEY_DRIVER_OPTION: &str = "encryption_key";
const ENCRYPTION_KEY_VALUE: &str = "ephemeral";
const METADATA_ENCRYPTION_KEY: &str = "encryptionKey";
const METADATA_FILESYSTEM_TYPE: &str = "filesystemType";
const METADATA_FS_GROUP: &str = "fsGroup";
const DISCARD_MOUNT_OPTION: &str = "discard";

/// Information about an ephemeral disk created on the host, needed for
/// sandbox-level cleanup.
#[derive(Debug, Clone)]
pub(crate) struct EphemeralDiskInfo {
    pub disk_path: PathBuf,
    pub source_path: String,
}

#[derive(Clone)]
pub(crate) struct BlockEmptyDirVolume {
    storage: Option<agent::Storage>,
    mount: oci::Mount,
    device_id: String,
    pub(crate) disk_info: EphemeralDiskInfo,
}

impl BlockEmptyDirVolume {
    pub(crate) async fn new(
        d: &RwLock<DeviceManager>,
        m: &oci::Mount,
        sid: &str,
        emptydir_mode: &str,
    ) -> Result<Self> {
        let encrypted = emptydir_mode == EMPTYDIR_MODE_BLOCK_ENCRYPTED;
        let discard_unmap = emptydir_mode == EMPTYDIR_MODE_BLOCK_PLAIN;
        let source = m
            .source()
            .as_ref()
            .ok_or_else(|| anyhow!("block emptyDir mount has no source"))?
            .display()
            .to_string();

        let emptydir_path = Path::new(&source);
        let disk_path = emptydir_path.join(DISK_IMG);

        // Stat the emptyDir now; kubelet sets its GID to the pod's fsGroup so
        // we need the value both for mountInfo.json metadata and for the agent
        // storage's fs_group field (which genpolicy validates exactly).
        let dir_gid = std::fs::metadata(emptydir_path)
            .with_context(|| format!("stat emptyDir {:?}", emptydir_path))?
            .gid();

        if !is_volume_mounted(&source) {
            let capacity = get_filesystem_capacity(emptydir_path)?;

            let f = std::fs::File::create(&disk_path)
                .with_context(|| format!("create sparse disk {:?}", disk_path))?;
            f.set_len(capacity)
                .with_context(|| format!("truncate sparse disk to {capacity} bytes"))?;

            let mut metadata = HashMap::new();
            if encrypted {
                metadata.insert(
                    METADATA_ENCRYPTION_KEY.to_string(),
                    ENCRYPTION_KEY_VALUE.to_string(),
                );
            } else {
                metadata.insert(METADATA_FILESYSTEM_TYPE.to_string(), "ext4".to_string());
            }
            if dir_gid != 0 {
                metadata.insert(METADATA_FS_GROUP.to_string(), dir_gid.to_string());
            }

            let mount_info = DirectVolumeMountInfo {
                volume_type: "blk".to_string(),
                device: disk_path.display().to_string(),
                fs_type: "ext4".to_string(),
                metadata,
                options: if discard_unmap {
                    vec![DISCARD_MOUNT_OPTION.to_string()]
                } else {
                    vec![]
                },
            };

            add_volume_mount_info(&source, &mount_info)
                .context("write direct-volume mountInfo.json")?;
        }

        let blkdev_info = get_block_device_info(d).await;
        let block_config = BlockConfigModern {
            path_on_host: disk_path.display().to_string(),
            driver_option: blkdev_info.block_device_driver,
            blkdev_aio: BlockDeviceAio::new(&blkdev_info.block_device_aio),
            num_queues: blkdev_info.num_queues,
            queue_size: blkdev_info.queue_size,
            logical_sector_size: blkdev_info.block_device_logical_sector_size,
            physical_sector_size: blkdev_info.block_device_physical_sector_size,
            discard_unmap,
            ..Default::default()
        };

        let device_info = do_handle_device(d, &DeviceConfig::BlockCfgModern(block_config))
            .await
            .context("plug block emptyDir block device")?;

        let (storage, mut mount, device_id) =
            crate::volume::utils::handle_block_volume(device_info, m, false, sid, "ext4")
                .await
                .context("handle block emptyDir block volume")?;

        // genpolicy generates a "bind" type mount for emptyDir volumes; keep
        // the OCI mount type as "bind" so the agent policy allows the request.
        mount.set_typ(Some("bind".to_string()));

        let mut storage = storage;
        if encrypted {
            storage.driver_options.push(format!(
                "{}={}",
                ENCRYPTION_KEY_DRIVER_OPTION, ENCRYPTION_KEY_VALUE
            ));
        } else {
            storage
                .driver_options
                .push(format!("{}=ext4", KATA_BLOCK_VOLUME_FS_TYPE));
            storage.options.push(DISCARD_MOUNT_OPTION.to_string());
        }
        storage.shared = true;

        // Mirror the Go runtime's handleBlkOCIMounts: the agent mounts the
        // block device at $(spath)/$(b64_device_id) which genpolicy expands to
        // kataGuestSandboxStorageDir + "/" + base64url(source).  That constant
        // is "/run/kata-containers/sandbox/storage" (== genpolicy's "spath"),
        // which is distinct from kata_guest_share_dir.  Using the passthrough
        // path would always fail the policy check.
        let b64_source = URL_SAFE.encode(storage.source.as_bytes());
        let agent_mount_point =
            format!("{}/storage/{}", DEFAULT_KATA_GUEST_SANDBOX_DIR, b64_source);
        storage.mount_point = agent_mount_point.clone();
        mount.set_source(Some(PathBuf::from(&agent_mount_point)));

        // Propagate the emptyDir directory GID as fs_group so that the agent
        // policy check (strict equality on fs_group) matches what genpolicy
        // generated from the pod's securityContext.fsGroup.
        if dir_gid != 0 {
            storage.fs_group = Some(agent::FSGroup {
                group_id: dir_gid,
                group_change_policy: agent::FSGroupChangePolicy::Always,
            });
        }

        let disk_info = EphemeralDiskInfo {
            disk_path,
            source_path: source,
        };

        Ok(Self {
            storage: Some(storage),
            mount,
            device_id,
            disk_info,
        })
    }
}

#[async_trait]
impl Volume for BlockEmptyDirVolume {
    fn get_volume_mount(&self) -> Result<Vec<oci::Mount>> {
        Ok(vec![self.mount.clone()])
    }

    fn get_storage(&self) -> Result<Vec<agent::Storage>> {
        let s = if let Some(s) = self.storage.as_ref() {
            vec![s.clone()]
        } else {
            vec![]
        };
        Ok(s)
    }

    async fn cleanup(&self, _device_manager: &RwLock<DeviceManager>) -> Result<()> {
        // Cleanup is deferred to sandbox teardown because the storage is shared
        // across all containers in the pod.
        Ok(())
    }

    fn get_device_id(&self) -> Result<Option<String>> {
        Ok(Some(self.device_id.clone()))
    }
}

pub(crate) fn is_block_emptydir_volume(m: &oci::Mount, emptydir_mode: &str) -> bool {
    if emptydir_mode != EMPTYDIR_MODE_BLOCK_ENCRYPTED && emptydir_mode != EMPTYDIR_MODE_BLOCK_PLAIN
    {
        return false;
    }
    // Kubelet always presents emptyDir mounts as "bind" type in the OCI spec.
    // Any other type means this is not a plain host-backed emptyDir, so skip it.
    let typ = match m.typ() {
        Some(t) => t,
        None => return false,
    };
    if typ != KATA_MOUNT_BIND_TYPE {
        return false;
    }
    match m.source() {
        Some(src) => is_host_empty_dir(&src.display().to_string()),
        None => false,
    }
}

fn get_filesystem_capacity(path: &Path) -> Result<u64> {
    let stat = statfs(path).with_context(|| format!("statfs {:?}", path))?;
    let total = stat.blocks() as u64 * stat.block_size() as u64;
    if total == 0 {
        return Err(anyhow!("filesystem at {:?} reports zero capacity", path));
    }
    Ok(total)
}
