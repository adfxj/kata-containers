// Copyright (c) 2022-2023 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;
use std::collections::HashSet;
use anyhow::Result;
use crate::utils::get_sandbox_path;
use serde::Deserialize;
use kata_sys_util::protection::GuestProtection;

// The socket used to connect to CH. This is used for CH API communications.
const CH_API_SOCKET_NAME: &str = "ch-api.sock";

// The socket that allows runtime-rs to connect direct through to the Kata
// Containers agent running inside the CH hosted VM.
const CH_VM_SOCKET_NAME: &str = "ch-vm.sock";

// Return the path for a _hypothetical_ API socket path:
// the path does *not* exist yet, and for this reason safe-path cannot be
// used.
pub fn get_api_socket_path(id: &str) -> Result<String> {
    let sandbox_path = get_sandbox_path(id);

    let path = [&sandbox_path, CH_API_SOCKET_NAME].join("/");

    Ok(path)
}

// Return the path for a _hypothetical_ sandbox specific VSOCK socket path:
// the path does *not* exist yet, and for this reason safe-path cannot be
// used.
pub fn get_vsock_path(id: &str) -> Result<String> {
    let sandbox_path = get_sandbox_path(id);

    let path = [&sandbox_path, CH_VM_SOCKET_NAME].join("/");

    Ok(path)
}

pub fn get_child_threads(pid: u32) -> HashSet<u32> {
    let mut result = HashSet::new();
    let path_name = format!("/proc/{}/task", pid);
    let path = Path::new(&path_name);

    if path.is_dir() {
        if let Ok(dir) = path.read_dir() {
            for entity in dir.flatten() {
                let tid_path = entity.path();
                let file_name = tid_path.file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or_default();

                if let Ok(tid) = file_name.parse::<u32>() {
                    let comm_path = tid_path.join("comm");
                    if let Ok(comm_content) = fs::read_to_string(comm_path) {
                        let thread_name = comm_content.trim();
                        if !thread_name.starts_with("vcpu") {
                            result.insert(tid);
                        }
                    } else {
                        result.insert(tid);
                    }
                }
            }
        }
    }
    result
}

#[derive(Deserialize, Debug)]
pub struct PciDeviceInfo {
    pub id: String,
    pub bdf: String,
}

// Returns true if the enabled guest protection is Intel TDX.
pub fn guest_protection_is_tdx(guest_protection_to_use: GuestProtection) -> bool {
    matches!(guest_protection_to_use, GuestProtection::Tdx)
}
