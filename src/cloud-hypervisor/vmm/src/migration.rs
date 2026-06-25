// Copyright © 2020 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{metadata, File};
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

use anyhow::anyhow;
use vm_migration::{MigratableError, Snapshot, protocol::MemoryRangeTable};

#[cfg(all(target_arch = "x86_64", feature = "guest_debug"))]
use crate::coredump::GuestDebuggableError;
use crate::vm::VmSnapshot;
use crate::vm_config::VmConfig;

pub const SNAPSHOT_STATE_FILE: &str = "state.json";
pub const SNAPSHOT_CONFIG_FILE: &str = "config.json";
pub const DIRYT_LOG_FILE: &str = "dirty-log.json";

pub fn url_to_path(url: &str) -> std::result::Result<PathBuf, MigratableError> {
    let path: PathBuf = url
        .strip_prefix("file://")
        .ok_or_else(|| {
            MigratableError::MigrateSend(anyhow!("Could not extract path from URL: {url}"))
        })
        .map(|s| s.into())?;

    if !path.is_dir() {
        return Err(MigratableError::MigrateSend(anyhow!(
            "Destination is not a directory: {path:?}"
        )));
    }

    Ok(path)
}

pub fn memory_blob_to_path(input: &str) -> std::result::Result<PathBuf, MigratableError> {
    let path = if input.starts_with("file://") {
        input.strip_prefix("file://")
            .ok_or_else(|| {
                MigratableError::MigrateSend(anyhow!("Could not extract memory blob path from URL: {input}"))
            })
            .map(|s| s.into())?
    } else if input.contains("://") {
        return Err(MigratableError::MigrateSend(anyhow!(
            "Unsupported memory blob URL scheme: {input}"
        )));
    } else {
        PathBuf::from(input)
    };

    if !path.is_absolute() {
        return Err(MigratableError::MigrateSend(anyhow!(
            "Memory blob path must be absolute: {path:?}"
        )));
    }

    let file_type = metadata(&path)
        .map_err(|e| {
            MigratableError::MigrateSend(anyhow!(
                "Failed to stat memory blob path {path:?}: {e}"
            ))
        })?
        .file_type();

    if file_type.is_dir() {
        return Err(MigratableError::MigrateSend(anyhow!(
            "Memory blob path must not be a directory: {path:?}"
        )));
    }

    if !(file_type.is_file() || file_type.is_block_device()) {
        return Err(MigratableError::MigrateSend(anyhow!(
            "Memory blob path must point to an existing regular file or block device: {path:?}"
        )));
    }

    Ok(path)
}

#[cfg(all(target_arch = "x86_64", feature = "guest_debug"))]
pub fn url_to_file(url: &str) -> std::result::Result<PathBuf, GuestDebuggableError> {
    let file: PathBuf = url
        .strip_prefix("file://")
        .ok_or_else(|| {
            GuestDebuggableError::Coredump(anyhow!("Could not extract file from URL: {url}"))
        })
        .map(|s| s.into())?;

    Ok(file)
}

pub fn recv_vm_config(source_url: &str) -> std::result::Result<VmConfig, MigratableError> {
    let mut vm_config_path = url_to_path(source_url)?;

    vm_config_path.push(SNAPSHOT_CONFIG_FILE);

    // Try opening the snapshot file
    let mut vm_config_file =
        File::open(vm_config_path).map_err(|e| MigratableError::MigrateReceive(e.into()))?;
    let mut bytes = Vec::new();
    vm_config_file
        .read_to_end(&mut bytes)
        .map_err(|e| MigratableError::MigrateReceive(e.into()))?;

    serde_json::from_slice(&bytes).map_err(|e| MigratableError::MigrateReceive(e.into()))
}

pub fn recv_vm_state(source_url: &str) -> std::result::Result<Snapshot, MigratableError> {
    let mut vm_state_path = url_to_path(source_url)?;

    vm_state_path.push(SNAPSHOT_STATE_FILE);

    // Try opening the snapshot file
    let mut vm_state_file =
        File::open(vm_state_path).map_err(|e| MigratableError::MigrateReceive(e.into()))?;
    let mut bytes = Vec::new();
    vm_state_file
        .read_to_end(&mut bytes)
        .map_err(|e| MigratableError::MigrateReceive(e.into()))?;

    serde_json::from_slice(&bytes).map_err(|e| MigratableError::MigrateReceive(e.into()))
}

pub fn recv_vm_dirty_log(source_url: &str) -> std::result::Result<MemoryRangeTable, MigratableError> {
    let mut vm_dirty_log_path = url_to_path(source_url)?;

    vm_dirty_log_path.push(DIRYT_LOG_FILE);

    // Try opening the snapshot file
    let mut vm_dirty_log_file =
        File::open(vm_dirty_log_path).map_err(|e| MigratableError::MigrateReceive(e.into()))?;
    let mut bytes = Vec::new();
    vm_dirty_log_file
        .read_to_end(&mut bytes)
        .map_err(|e| MigratableError::MigrateReceive(e.into()))?;

    serde_json::from_slice(&bytes).map_err(|e| MigratableError::MigrateReceive(e.into()))
}

pub fn get_vm_snapshot(snapshot: &Snapshot) -> std::result::Result<VmSnapshot, MigratableError> {
    if let Some(snapshot_data) = snapshot.snapshot_data.as_ref() {
        return snapshot_data.to_state();
    }

    Err(MigratableError::Restore(anyhow!(
        "Could not find VM config snapshot section"
    )))
}

#[cfg(test)]
mod tests {
    use super::{memory_blob_to_path, url_to_path};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ch-migration-{}-{}", name, nanos))
    }

    #[test]
    fn memory_blob_accepts_absolute_regular_file_and_file_url() {
        let file_path = unique_temp_path("memory-blob");
        fs::write(&file_path, b"memory").unwrap();

        let file_url = format!("file://{}", file_path.display());
        assert_eq!(
            memory_blob_to_path(file_path.to_str().unwrap()).unwrap(),
            file_path
        );
        assert_eq!(memory_blob_to_path(&file_url).unwrap(), file_path);

        fs::remove_file(file_path).unwrap();
    }

    #[test]
    fn memory_blob_rejects_relative_paths_directories_and_invalid_schemes() {
        let dir_path = unique_temp_path("memory-dir");
        fs::create_dir_all(&dir_path).unwrap();
        let missing_path = unique_temp_path("missing");

        assert!(memory_blob_to_path("relative/path").is_err());
        assert!(memory_blob_to_path(dir_path.to_str().unwrap()).is_err());
        assert!(memory_blob_to_path(missing_path.to_str().unwrap()).is_err());
        assert!(memory_blob_to_path("dev:///tmp/memory").is_err());

        fs::remove_dir_all(dir_path).unwrap();
    }

    #[test]
    fn url_to_path_still_requires_directory_file_url() {
        let dir_path = unique_temp_path("snapshot-dir");
        fs::create_dir_all(&dir_path).unwrap();
        let file_url = format!("file://{}", dir_path.display());

        assert_eq!(url_to_path(&file_url).unwrap(), dir_path);
        assert!(url_to_path(dir_path.to_str().unwrap()).is_err());

        fs::remove_dir_all(dir_path).unwrap();
    }
}