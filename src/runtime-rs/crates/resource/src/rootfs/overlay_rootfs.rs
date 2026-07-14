// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::os::unix::fs::OpenOptionsExt;
use std::{fs::{self, OpenOptions}, sync::Arc};
use std::path::Path;
use std::process::Command;

use agent::Storage;
use anyhow::{Context, Ok, Result};
use async_trait::async_trait;
use kata_hypervisor::{device::{device_manager::{do_handle_device, get_overlayfs_block_device, get_block_device_info, DeviceManager}, DeviceConfig, DeviceType}, BlockConfig};
use kata_sys_util::{fs::reflink_copy, mount::{umount_timeout, Mounter}};
use kata_types::mount::Mount;
use oci_spec::runtime as oci;
use tokio::sync::RwLock;

use super::{Rootfs, ROOTFS, TYPE_OVERLAY_FS};
use crate::share_fs::{ShareFs, ShareFsRootfsConfig};

// Used for overlay rootfs
pub(crate) const OVERLAY_ROOTFS_TYPE: &str = "io.katacontainers.overlayfs";
pub(crate) const OVERLAY_ROOTFS_SIZE: &str = "io.katacontainers.overlayfs.size";

// overlay upper blk mount point
const OVERLAY_BLK_POINT: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_point";
//  overlay upper blk source path
const OVERLAY_BLK_SOURCE: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_src";

const OVERLAY_OPTION: &str = "io.katacontainers.fs-opt.overlay-rw";

const KATA_OVERLAY_GUEST_PATH: &str = "/run/kata-containers/overlay";
const KATA_OVERLAY_DEV_TYPE: &str = "overlayfs";
const KATA_OVERLAY_DIR: &str = "merge";

const DEFAULT_PERMISSIONS: u32 = 0o644;

pub(crate) struct OverlayRootfs {
    guest_path: String,
    share_fs: Arc<dyn ShareFs>,
    config: ShareFsRootfsConfig,
    rootfs: Storage,
    device_id: String,
}

impl OverlayRootfs {
    pub async fn new(
        d: &RwLock<DeviceManager>,
        share_fs: &Arc<dyn ShareFs>,
        cid: &str,
        _sid: &str,
        bundle_path: &str,
        rootfs: Option<&Mount>,
        template_storage: &str,
        dst_storage_size: u64,
    ) -> Result<Self> {
        let mut device_id: String = "".to_owned();
        let mut driver_options: Vec<String> = Vec::new();
        if let Some(block_dev) = get_overlayfs_block_device(d).await {
            if let DeviceType::Block(device) = block_dev {
                device_id = device.device_id;
            }
        } else {
            let template_storage_dir = get_parent_dir(template_storage).take().unwrap();
            let dst_template_storage = format!("{}/{}.img", template_storage_dir, cid);

            let mut new_template_storage = String::from(template_storage);
            // if needed to use a new size storag, create a new template file by template storage
            if dst_storage_size != 0 {
                new_template_storage = format!("{}/new_{}.img", template_storage_dir, cid);
                create_new_template_storage(&new_template_storage, dst_storage_size).map_err( |error| {
                    error!(sl!(), "Failed to create template storage: {}", new_template_storage);
                    let _ = fs::remove_file(&new_template_storage);
                    error
                })?;
            }
            // reflink_copy
            reflink_copy(&new_template_storage, &dst_template_storage).context(format!(
                "reflink copy from {} to {}", new_template_storage, dst_template_storage))?;

            // rm new_template_storage if needed
            if dst_storage_size != 0 {
                let _ = fs::remove_file(&new_template_storage);
            }

            let blkdev_info = get_block_device_info(d).await;

            let block_device_config = &mut BlockConfig {
                path_on_host: dst_template_storage.clone(),
                driver_option: blkdev_info.block_device_driver.clone(),
                blkdev_aio: kata_hypervisor::BlockDeviceAio::new(&blkdev_info.block_device_aio),
                num_queues: blkdev_info.num_queues,
                queue_size: blkdev_info.queue_size,
                logical_sector_size: blkdev_info.block_device_logical_sector_size,
                physical_sector_size: blkdev_info.block_device_physical_sector_size,
                is_overlayfs: true,
                ..Default::default()
            };

            // create and insert block device into Kata VM
            let result = do_handle_device(d, &DeviceConfig::BlockCfg(block_device_config.clone())).await;

            // remove dst_template_storage
            let _ = fs::remove_file(&dst_template_storage);

            let device_info = result.context("do handle device failed.")?;

            // get path on guest
             let mut vir_path: String = "".to_owned();
            if let DeviceType::Block(device) = device_info {
                vir_path = device.config.virt_path;
                device_id = device.device_id;
            }

            driver_options.push(
                OVERLAY_BLK_SOURCE.to_owned() + &"=".to_string() + &vir_path,
            );

            driver_options.push(
                OVERLAY_BLK_POINT.to_owned() + &"=".to_string()
                    + KATA_OVERLAY_GUEST_PATH,
            );
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

fn get_parent_dir(path_str: &str) -> Option<&str> {
    let path = Path::new(path_str);
    path.parent()
        .and_then(|p| p.to_str())
}

fn create_new_template_storage(new_storage: &str, new_size_bytes: u64) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(DEFAULT_PERMISSIONS)
        .open(new_storage)
        .with_context(|| format!("Failed to create file: {}", new_storage))?;

    file.set_len(new_size_bytes).with_context(|| format!("Failed to set file size to {} bytes", new_size_bytes))?;

    let mut cmd = Command::new("mkfs.ext4");

    cmd.arg(new_storage);

    let status = cmd.status().context("Failed to execute mkfs.ext4")?;
    if !status.success() {
        error!(sl!(), "mkfs.ext4 failed with exit code: {:?}", status.code());
    }

    Ok(())
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
