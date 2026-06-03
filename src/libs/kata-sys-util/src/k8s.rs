// Copyright (c) 2019-2021 Alibaba Cloud
// Copyright (c) 2019-2021 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

//! Utilities to support Kubernetes (K8s).
//!
//! This module depends on kubelet internal implementation details, a better way is needed
//! to detect K8S EmptyDir medium type from `oci::spec::Mount` objects.

use kata_types::mount;
use oci_spec::runtime::{Mount, Spec};

use crate::mount::get_linux_mount_info;

pub use kata_types::k8s::is_empty_dir;

/// Check whether a given volume is an ephemeral volume.
///
/// For k8s, there are generally two types of ephemeral volumes: one is the
/// volume used as /dev/shm of the container, and the other is the
/// emptydir volume based on the memory type. Both types of volumes
/// are based on tmpfs mount volumes, so we classify them as ephemeral
/// volumes and can be setup in the guest; For the other volume based on tmpfs
/// which would contain some initial files we cound't deal them as ephemeral and
/// should be passed using share fs.
pub fn is_ephemeral_volume(mount: &Mount) -> bool {
    matches!(
        (
            mount.typ().as_deref(),
            mount.source().as_deref().and_then(|s| s.to_str()),
            mount.destination(),

        ),
        (Some("bind"), Some(source), _dest) if get_linux_mount_info(source).is_ok_and(|info| info.fs_type == "tmpfs") &&
            is_empty_dir(source))
}

/// Check whether the given path is a kubernetes empty-dir volume of medium "default".
///
/// K8s `EmptyDir` volumes are directories on the host. If the fs type is tmpfs, it's a ephemeral
/// volume instead of a `EmptyDir` volume.
pub fn is_host_empty_dir(path: &str) -> bool {
    if !is_empty_dir(path) {
        return false;
    }

    match get_linux_mount_info(path) {
        Ok(info) => info.fs_type != "tmpfs",
        Err(crate::mount::Error::NoMountEntry(_)) => true,
        Err(_) => false,
    }
}

// update_ephemeral_storage_type sets the mount type to 'ephemeral'
// if the mount source path is provisioned by k8s for ephemeral storage.
// For the given pod ephemeral volume is created only once
// backed by tmpfs inside the VM. For successive containers
// of the same pod the already existing volume is reused.
pub fn update_ephemeral_storage_type(
    oci_spec: &mut Spec,
    disable_guest_empty_dir: bool,
    emptydir_mode: &str,
) {
    use kata_types::config::{EMPTYDIR_MODE_BLOCK_ENCRYPTED, EMPTYDIR_MODE_BLOCK_PLAIN};

    if let Some(mounts) = oci_spec.mounts_mut() {
        for m in mounts.iter_mut() {
            if let Some(typ) = &m.typ() {
                if mount::is_kata_guest_mount_volume(typ) {
                    continue;
                }
            }

            if let Some(source) = &m.source() {
                let mnt_src = &source.display().to_string();
                if is_ephemeral_volume(m) {
                    m.set_typ(Some(String::from(mount::KATA_EPHEMERAL_VOLUME_TYPE)));
                }
                // When a block mode is active, host emptyDirs must stay as
                // "bind" so the block emptyDir volume handler can
                // intercept them in the volume dispatch chain.
                if is_host_empty_dir(mnt_src)
                    && !disable_guest_empty_dir
                    && emptydir_mode != EMPTYDIR_MODE_BLOCK_ENCRYPTED
                    && emptydir_mode != EMPTYDIR_MODE_BLOCK_PLAIN
                {
                    m.set_typ(Some(mount::KATA_K8S_LOCAL_STORAGE_TYPE.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kata_types::config::{
        EMPTYDIR_MODE_BLOCK_ENCRYPTED, EMPTYDIR_MODE_BLOCK_PLAIN, EMPTYDIR_MODE_SHARED_FS,
    };
    use std::path::PathBuf;

    fn spec_with_host_empty_dir(source: PathBuf) -> Spec {
        let mut mount = Mount::default();
        mount.set_typ(Some("bind".to_string()));
        mount.set_source(Some(source));

        let mut spec = Spec::default();
        spec.set_mounts(Some(vec![mount]));
        spec
    }

    #[test]
    fn test_update_ephemeral_storage_type_host_empty_dir_modes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let empty_dir = temp_dir
            .path()
            .join("pods")
            .join("pod-id")
            .join("volumes")
            .join("kubernetes.io~empty-dir")
            .join("data");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let mut spec = spec_with_host_empty_dir(empty_dir.clone());
        update_ephemeral_storage_type(&mut spec, false, EMPTYDIR_MODE_SHARED_FS);
        assert_eq!(
            spec.mounts().as_ref().unwrap()[0].typ().as_deref(),
            Some(mount::KATA_K8S_LOCAL_STORAGE_TYPE)
        );

        let mut spec = spec_with_host_empty_dir(empty_dir.clone());
        update_ephemeral_storage_type(&mut spec, false, EMPTYDIR_MODE_BLOCK_ENCRYPTED);
        assert_eq!(
            spec.mounts().as_ref().unwrap()[0].typ().as_deref(),
            Some("bind")
        );

        let mut spec = spec_with_host_empty_dir(empty_dir.clone());
        update_ephemeral_storage_type(&mut spec, false, EMPTYDIR_MODE_BLOCK_PLAIN);
        assert_eq!(
            spec.mounts().as_ref().unwrap()[0].typ().as_deref(),
            Some("bind")
        );

        let mut spec = spec_with_host_empty_dir(empty_dir);
        update_ephemeral_storage_type(&mut spec, true, EMPTYDIR_MODE_SHARED_FS);
        assert_eq!(
            spec.mounts().as_ref().unwrap()[0].typ().as_deref(),
            Some("bind")
        );
    }
}
