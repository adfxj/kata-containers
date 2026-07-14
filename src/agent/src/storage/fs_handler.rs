// Copyright (c) 2019 Ant Financial
// Copyright (c) 2023 Alibaba Cloud
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::storage::{common_storage_handler, new_device, StorageContext, StorageHandler};
use anyhow::{anyhow, Context, Result};
use kata_types::device::{DRIVER_OVERLAYFS_TYPE, DRIVER_VIRTIOFS_TYPE};
use kata_types::mount::{StorageDevice, KATA_VOLUME_OVERLAYFS_CREATE_DIR};
use protocols::agent::Storage;
use tracing::instrument;

const KATA_OVERLAY_BLK_POINT: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_point";
const KATA_OVERLAY_BLK_SOURCE: &str = "io.katacontainers.rootfs.overlayfs.mount_blk_src";
const KATA_OVERLAY: &str = "overlay";

const FS_TYPE_EXT4: &str = "ext4";

#[derive(Debug)]
pub struct OverlayfsHandler {}

#[async_trait::async_trait]
impl StorageHandler for OverlayfsHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_OVERLAYFS_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        mut storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let overlay_blk_point = &(KATA_OVERLAY_BLK_POINT.to_string() + "=");
        let overlay_blk_source = &(KATA_OVERLAY_BLK_SOURCE.to_string() + "=");
        for driver_option in &storage.driver_options {
            if let Some(point) = driver_option
                .as_str()
                .strip_prefix(overlay_blk_point)
            {
                for driver_option in &storage.driver_options {
                    if let Some(src) = driver_option
                        .as_str()
                        .strip_prefix(overlay_blk_source)
                    {
                        let blk_storage = Storage {
                            source: src.to_string(),
                            mount_point: point.to_string(),
                            fstype: FS_TYPE_EXT4.to_string(),
                            ..Default::default()
                        };

                        let _ = common_storage_handler(ctx.logger, &blk_storage)?;
                    }
                }
            }
        }

        if storage
            .options
            .iter()
            .any(|e| e == "io.katacontainers.fs-opt.overlay-rw")
        {
            let cid = ctx
                .cid
                .clone()
                .ok_or_else(|| anyhow!("No container id in rw overlay"))?;
            let cpath = Path::new(crate::rpc::CONTAINER_BASE).join(KATA_OVERLAY).join(cid);
            let work = cpath.join("work");
            let upper = cpath.join("upper");

            fs::create_dir_all(&work).context("Creating overlay work directory")?;
            fs::create_dir_all(&upper).context("Creating overlay upper directory")?;

            let cpath = Path::new(&storage.mount_point);
            fs::create_dir_all(&cpath).context("Creating overlay merged directory")?;

            storage.fstype = "overlay".into();
            storage
                .options
                .push(format!("upperdir={}", upper.to_string_lossy()));
            storage
                .options
                .push(format!("workdir={}", work.to_string_lossy()));
        }

        storage
            .options
            .retain(|opt| opt != "io.katacontainers.fs-opt.overlay-rw");

        let overlay_create_dir_prefix = &(KATA_VOLUME_OVERLAYFS_CREATE_DIR.to_string() + "=");
        for driver_option in &storage.driver_options {
            if let Some(dir) = driver_option
                .as_str()
                .strip_prefix(overlay_create_dir_prefix)
            {
                fs::create_dir_all(dir).context("Failed to create directory")?;
            }
        }
        let path = common_storage_handler(ctx.logger, &storage)?;
        new_device(path)
    }
}

#[derive(Debug)]
pub struct VirtioFsHandler {}

#[async_trait::async_trait]
impl StorageHandler for VirtioFsHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_VIRTIOFS_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let path = common_storage_handler(ctx.logger, &storage)?;
        new_device(path)
    }
}
