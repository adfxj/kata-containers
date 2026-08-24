// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::sync::Arc;

use agent::Storage;
use anyhow::{Context, Ok, Result};
use async_trait::async_trait;
use kata_hypervisor::device::{device_manager::DeviceManager, DeviceType};
use kata_sys_util::mount::{umount_timeout, Mounter};
use kata_types::mount::Mount;
use oci_spec::runtime as oci;
use tokio::sync::RwLock;

use super::{Rootfs, ROOTFS, TYPE_OVERLAY_FS};
use crate::share_fs::{ShareFs, ShareFsRootfsConfig};

// overlay upper blk mount point
const OVERLAY_BLK_POINT: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_point";
//  overlay upper blk source path
const OVERLAY_BLK_SOURCE: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_src";

const OVERLAY_OPTION: &str = "io.katacontainers.fs-opt.overlay-rw";

const KATA_OVERLAY_GUEST_PATH: &str = "/run/kata-containers/overlay";
const KATA_OVERLAY_DEV_TYPE: &str = "overlayfs";
const KATA_OVERLAY_DIR: &str = "merge";

pub(crate) struct OverlayRootfs {
    guest_path: String,
    share_fs: Arc<dyn ShareFs>,
    config: ShareFsRootfsConfig,
    rootfs: Storage,
    device_id: String,
}

impl OverlayRootfs {
    pub async fn new(
        share_fs: &Arc<dyn ShareFs>,
        cid: &str,
        _sid: &str,
        bundle_path: &str,
        rootfs: Option<&Mount>,
        block_dev: DeviceType,
    ) -> Result<Self> {
        let mut device_id: String = "".to_owned();
        let mut driver_options: Vec<String> = Vec::new();
        if let DeviceType::Block(device) = block_dev {
            device_id = device.device_id;
            if device.config.is_overlayfs {
                // get path on guest
                let vir_path = device.config.virt_path;

                driver_options.push(
                    OVERLAY_BLK_SOURCE.to_owned() + &"=".to_string() + &vir_path,
                );

                driver_options.push(
                    OVERLAY_BLK_POINT.to_owned() + &"=".to_string()
                        + KATA_OVERLAY_GUEST_PATH,
                );
            }
        }

        let bundle_rootfs = if let Some(rootfs) = rootfs {
            let bundle_rootfs = format!("{}/{}", bundle_path, ROOTFS);
            rootfs.mount(&bundle_rootfs).context(format!(
                "mount rootfs from {:?} to {}",
                &rootfs, &bundle_rootfs
            ))?;
            bundle_rootfs
        } else {
            bundle_path.to_string()
        };

        // mount share fs
        let share_fs_mount = share_fs.get_share_fs_mount();
        let config = ShareFsRootfsConfig {
            cid: cid.to_string(),
            source: bundle_rootfs.to_string(),
            target: ROOTFS.to_string(),
            readonly: false,
            is_rafs: false,
        };

        let mount_result = share_fs_mount
            .share_rootfs(&config)
            .await
            .context("share rootfs")?;

        let mut options: Vec<String> = Vec::new();
        options.push(
            "lowerdir=".to_string()
                + &mount_result.guest_path,
        );
        options.push(
            OVERLAY_OPTION.to_owned(),
        );
        options.push("index=off".to_string());

        Ok(OverlayRootfs {
            guest_path: format!("{}/{}/{}", KATA_OVERLAY_GUEST_PATH, cid, KATA_OVERLAY_DIR),
            share_fs: Arc::clone(share_fs),
            config,
            rootfs: Storage {
                driver: KATA_OVERLAY_DEV_TYPE.to_string(),
                source: TYPE_OVERLAY_FS.to_string(),
                fs_type: TYPE_OVERLAY_FS.to_string(),
                options,
                mount_point: format!("{}/{}/{}", KATA_OVERLAY_GUEST_PATH, cid, KATA_OVERLAY_DIR),
                driver_options,
                ..Default::default()
            },
            device_id,
        })
    }
}

#[async_trait]
impl Rootfs for OverlayRootfs {
    async fn get_guest_rootfs_path(&self) -> Result<String> {
        Ok(self.guest_path.clone())
    }

    async fn get_rootfs_mount(&self) -> Result<Vec<oci::Mount>> {
        Ok(vec![])
    }

    async fn get_storage(&self) -> Option<Vec<Storage>> {
        Some(vec![self.rootfs.clone()])
    }

    async fn get_device_id(&self) -> Result<Option<String>> {
        Ok(Some(self.device_id.clone()))
    }

    async fn cleanup(&self, _device_manager: &RwLock<DeviceManager>) -> Result<()> {
        // Umount the mount point shared to guest
        let share_fs_mount = self.share_fs.get_share_fs_mount();
        share_fs_mount
            .umount_rootfs(&self.config)
            .await
            .context("umount shared rootfs")?;

        // Umount the bundle rootfs
        umount_timeout(&self.config.source, 0).context("umount bundle rootfs")?;

        // No need to remove device from hypervisor, as it maybe used by any other container.
        // remove until hypervisor is stopped
        Ok(())
    }
}
