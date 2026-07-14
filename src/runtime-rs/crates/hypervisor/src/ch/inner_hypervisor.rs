// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use std::convert::TryInto;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::convert::TryFrom;
use std::cmp::Ordering;
use std::sync::{Arc, RwLock};
use tokio::task;
use tokio::time::Duration;

use super::inner::CloudHypervisorInner;
use super::convert::{NamedHypervisorConfig, RestoreConfigInner};
use super::utils::{
    get_api_socket_path, get_vsock_path, guest_protection_is_tdx
};
use crate::DeviceType;
use crate::kernel_param::KernelParams;
use crate::MemoryConfig;
use crate::selinux;
use crate::utils::create_dir_all_with_inherit_owner;
use crate::utils::{get_jailer_root, get_sandbox_path};
use crate::{VM_ROOTFS_DRIVER_BLK, VM_TEMPLATE_SIZE};
use crate::{VcpuThreadIds, VmmState};
use anyhow::{anyhow, Context, Ok, Result};
use std::collections::HashSet;
use kata_sys_util::protection::{available_guest_protection, GuestProtection};
use kata_types::capabilities::{Capabilities, CapabilityBits};
use kata_types::config::default::DEFAULT_CH_ROOTFS_TYPE;
use kata_types::config::PASSFD_LISTENER_PORT;
use lazy_static::lazy_static;
use vmm::{
    api::{VmmPingResponse, VmResizeData},
    SnapshotConfig,
    vm_config::VmConfig,
    config::RestoreConfig,
};

/// Number of milliseconds to wait before retrying a CH operation.
const CH_POLL_TIME_MS: u64 = 50;

// The name of the CH build-time feature for Intel TDX.
const CH_FEATURE_TDX: &str = "tdx";

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum GuestProtectionError {
    #[error("guest protection requested but no guest protection available")]
    NoProtectionAvailable,

    // LIMITATION: Current CH TDX limitation.
    //
    // When built to support TDX, if Cloud Hypervisor determines the host
    // system supports TDX, it can only create TD's (as opposed to VMs).
    // Hence, on a TDX capable system, confidential_guest *MUST* be set to
    // "true".
    #[error("TDX guest protection available and must be used with Cloud Hypervisor (set 'confidential_guest=true')")]
    TDXProtectionMustBeUsedWithCH,

    // TDX is the only tested CH protection currently.
    #[error("Expected TDX protection, found {0}")]
    ExpectedTDXProtection(GuestProtection),
}

impl CloudHypervisorInner {
    async fn start_hypervisor(&mut self, _timeout_secs: i32) -> Result<()> {
        self.cloud_hypervisor_launch()
            .context("launch failed")?;

        self.cloud_hypervisor_check_running()
            .await
            .context("hypervisor running check failed")?;

        if guest_protection_is_tdx(self.guest_protection_to_use.clone()) {
            if let Some(features) = &self.ch_features {
                if !features.contains(&CH_FEATURE_TDX.to_string()) {
                    return Err(anyhow!("Cloud Hypervisor is not built with TDX support"));
                }
            }
        }

        Ok(())
    }

    async fn get_kernel_params(&self) -> Result<String> {
        let cfg = &self.config;

        let enable_debug = cfg.debug_info.enable_debug;

        let confidential_guest = cfg.security_info.confidential_guest;

        // Note that the configuration option hypervisor.block_device_driver is not used.
        let rootfs_driver = if confidential_guest {
            // PMEM is not available with TDX.
            VM_ROOTFS_DRIVER_BLK
        } else {
            &cfg.boot_info.vm_rootfs_driver
        };

        let rootfs_type = match cfg.boot_info.rootfs_type.is_empty() {
            true => DEFAULT_CH_ROOTFS_TYPE,
            false => &cfg.boot_info.rootfs_type,
        };

        // Start by adding the default set of kernel parameters.
        let mut params = KernelParams::new(enable_debug);

        #[cfg(target_arch = "x86_64")]
        let console_param_debug = KernelParams::from_string("console=ttyS0,115200n8");

        #[cfg(target_arch = "aarch64")]
        let console_param_debug = KernelParams::from_string("console=ttyAMA0,115200n8");

        let mut rootfs_params = KernelParams::new_rootfs_kernel_params(
            &cfg.boot_info.kernel_verity_params,
            rootfs_driver,
            rootfs_type,
            true,
        )?;

        let mut console_params = if enable_debug {
            if confidential_guest {
                KernelParams::from_string("console=hvc0")
            } else {
                console_param_debug
            }
        } else {
            KernelParams::from_string("quiet")
        };

        if let Some(passfd_listener_port) = self.passfd_listener_port {
            params.append(&mut KernelParams::from_string(&format!(
                "{}={}",
                PASSFD_LISTENER_PORT, passfd_listener_port
            )));
        }

        params.append(&mut console_params);

        // Add the rootfs device
        params.append(&mut rootfs_params);

        // Now add some additional options required for CH
        let extra_options = [
            "no_timer_check",             // Do not Check broken timer IRQ resources
            "noreplace-smp",              // Do not replace SMP instructions
            "systemd.log_target=console", // Send logging output to the console
        ];

        let mut extra_params = KernelParams::from_string(&extra_options.join(" "));
        params.append(&mut extra_params);

        // Finally, add the user-specified options at the end
        // (so they will take priority).
        params.append(&mut KernelParams::from_string(&cfg.boot_info.kernel_params));

        let kernel_params = params.to_string()?;

        Ok(kernel_params)
    }

    async fn boot_vm(&mut self) -> Result<()> {
        let (shared_fs_devices, network_devices, host_devices, protection_device) =
            self.get_shared_devices().await?;

        create_dir_all_with_inherit_owner(&self.vm_path.clone(), 0o750)
            .context("failed to create sandbox path")?;

        let vsock_socket_path = get_vsock_path(&self.id)?;

        debug!(
            sl!(),
            "generic Hypervisor configuration: {:?}",
            self.config.clone()
        );

        let kernel_params = self.get_kernel_params().await?;

        let mut config_clone = self.config.clone();
        if self.config.vm_template.boot_to_be_template {
            config_clone.cpu_info.default_vcpus = 1.0;
            config_clone.memory_info.default_memory = VM_TEMPLATE_SIZE;
        }

        let named_cfg = NamedHypervisorConfig {
            kernel_params,
            sandbox_path: self.vm_path.clone(),
            vsock_socket_path,
            cfg: config_clone,
            guest_protection_to_use: self.guest_protection_to_use.clone(),
            shared_fs_devices,
            network_devices,
            host_devices,
            protection_device,
        };

        let cfg: VmConfig = VmConfig::try_from(named_cfg)?;

        let serialised = serde_json::to_string(&cfg)?;

        debug!(
            sl!(),
            "CH specific VmConfig configuration (JSON): {:?}", serialised
        );

        self.vmm_instance.create_vm_instance(cfg).await.context("failed to create vm")?;

        self.vmm_instance.start_vm_instance().await.context("failed to start vm")?;
 
        Ok(())
    }

    async fn boot_from_template(&mut self) -> Result<()> {
        let (_shared_fs_devices, network_devices, host_devices, _protection_device) =
            self.get_shared_devices().await?;

        create_dir_all_with_inherit_owner(self.vm_path.clone(), 0o750)
            .context("failed to create sandbox path")?;

        info!(sl!(), "Boot from template");

        let restore_cfg = RestoreConfigInner::new(
            Some(self.config.factory.template_path.clone()),
            get_vsock_path(&self.id)?,
        );

        let cfg = RestoreConfig::try_from(restore_cfg)?;

        self.vmm_instance.vm_restore(cfg).await.context("failed to restore vm")?;

        if let Some(mut net_configs) = network_devices {
            while let Some(net_config) = net_configs.pop() {
                self.vmm_instance.vm_add_net(net_config).await.context("attach net")?;
            }
        }

        if let Some(mut device_configs) = host_devices {
            while let Some(device_config) = device_configs.pop() {
                self.vmm_instance.vm_add_device(device_config).await.context("attach device")?;
            }
        }

        Ok(())
    }

    async fn cloud_hypervisor_check_running(&mut self) -> Result<()> {
        let timeout_secs = self.timeout_secs;

        let timeout_msg = format!(
            "API socket connect timed out after {} seconds",
            timeout_secs
        );

        let join_handle = self.cloud_hypervisor_ping_until_ready(CH_POLL_TIME_MS);

        tokio::time::timeout(Duration::new(timeout_secs as u64, 0), join_handle)
            .await
            .context(timeout_msg)?
    }

    fn cloud_hypervisor_launch(&mut self) -> Result<()> {
        let cfg = &self.config;

        let disable_seccomp = cfg.security_info.disable_seccomp;

        let netns = self.netns.clone();
        if self.netns.is_some() {
            info!(
                sl!(),
                "set netns for vmm : {:?}",
                self.netns.as_ref().unwrap()
            );
        }

        let secomp_value = match disable_seccomp {
                true => Some("true"),
                false => Some("false"),
        };

        let api_socket = if cfg.debug_info.enable_debug {
            let api_socket_path = get_api_socket_path(&self.id)?;
            Some(api_socket_path)
        } else {
            None
        };

        self.vmm_instance.run_vmm_server(&self.id, netns, secomp_value, &api_socket).context("start vmm server")?;

        Ok(())
    }

    // Check the specified ping API response to see if it contains CH's
    // build-time features list. If so, save them.
    async fn handle_ch_build_features(&mut self, ping_response: VmmPingResponse) -> Result<()> {
        self.ch_features = Some(ping_response.features);

        Ok(())
    }

    async fn cloud_hypervisor_ping_until_ready(&mut self, _poll_time_ms: u64) -> Result<()> {
        loop {
            let response = self.vmm_instance.vmm_ping().await.context("failed to ping vmm");

            if let core::result::Result::Ok(response) = response {
                self.handle_ch_build_features(response).await?;
                break;
            }

            tokio::time::sleep(Duration::from_millis(CH_POLL_TIME_MS)).await;
        }

        Ok(())
    }

    pub(crate) async fn prepare_vm(
        &mut self,
        id: &str,
        netns: Option<String>,
        selinux_label: Option<String>,
    ) -> Result<()> {
        self.id = id.to_string();
        self.state = VmmState::NotReady;

        self.setup_environment().await?;

        self.handle_guest_protection().await?;

        self.netns = netns;

        if !self.hypervisor_config().disable_selinux {
            if let Some(label) = selinux_label.as_ref() {
                self.config.security_info.selinux_label = Some(label.to_string());
                selinux::set_exec_label(label).context("failed to set SELinux process label")?;
            }
        }

        Ok(())
    }

    // Check if guest protection is available and also check if the user
    // actually wants to use it.
    //
    // Note: This method must be called as early as possible since after this
    // call, if confidential_guest is set, a confidential
    // guest will be created.
    async fn handle_guest_protection(&mut self) -> Result<()> {
        let cfg = &self.config;

        let confidential_guest = cfg.security_info.confidential_guest;

        if confidential_guest {
            info!(sl!(), "confidential guest requested");
        }

        let protection =
            task::spawn_blocking(|| -> Result<GuestProtection> { get_guest_protection() })
                .await??;

        self.guest_protection_to_use = protection.clone();

        info!(sl!(), "guest protection {:?}", protection.to_string());

        if confidential_guest {
            if protection == GuestProtection::NoProtection {
                // User wants protection, but none available.
                return Err(anyhow!(GuestProtectionError::NoProtectionAvailable));
            } else if let GuestProtection::Tdx = protection {
                info!(sl!(), "guest protection available and requested"; "guest-protection" => protection.to_string());
            } else {
                return Err(anyhow!(GuestProtectionError::ExpectedTDXProtection(
                    protection
                )));
            }
        } else if protection == GuestProtection::NoProtection {
            debug!(sl!(), "no guest protection available");
        } else if let GuestProtection::Tdx = protection {
            // CH requires TDX protection to be used.
            return Err(anyhow!(GuestProtectionError::TDXProtectionMustBeUsedWithCH));
        } else {
            info!(sl!(), "guest protection available but not requested"; "guest-protection" => protection.to_string());
        }

        Ok(())
    }

    async fn setup_environment(&mut self) -> Result<()> {
        // run_dir and vm_path are the same (shared)
        self.run_dir = get_sandbox_path(&self.id);
        self.vm_path = self.run_dir.to_string();

        create_dir_all_with_inherit_owner(&self.run_dir, 0o750)
            .with_context(|| anyhow!("failed to create sandbox directory {}", self.run_dir))?;

        if !self.jailer_root.is_empty() {
            create_dir_all_with_inherit_owner(self.jailer_root.as_str(), 0o750)
                .map_err(|e| anyhow!("Failed to create dir {} err : {:?}", self.jailer_root, e))?;
        }

        Ok(())
    }

    pub(crate) async fn start_vm(&mut self, timeout_secs: i32) -> Result<()> {
        self.timeout_secs = timeout_secs;
        self.start_hypervisor(self.timeout_secs).await?;

        self.state = VmmState::VmmServerReady;

        if self.config.vm_template.boot_from_template && !self.config.memory_info.enable_hugepages {
            self.boot_from_template().await.map_err(|error| {
                error!(sl!(), "boot vm error: {:?}", error);
                if let Err(err) = futures::executor::block_on(self.stop_vm()) {
                    error!(sl!(), "failed to call stop_vm: {:?}", err);
                }
                error
            })?;
        } else {
            self.boot_vm().await.map_err(|error| {
                error!(sl!(), "boot vm error: {:?}", error);
                if let Err(err) = futures::executor::block_on(self.stop_vm()) {
                    error!(sl!(), "failed to call stop_vm: {:?}", err);
                }
                error
            })?;
        }

        self.state = VmmState::VmRunning;

        Ok(())
    }

    pub(crate) async fn stop_vm(&mut self) -> Result<()> {
        // If the container workload exits, this method gets called. However,
        // the container manager always makes a ShutdownContainer request,
        // which results in this method being called potentially a second
        // time. Without this check, we'll return an error representing EPIPE
        // since the CH API socket is at that point invalid.
        if self.state == VmmState::VmRunning {
            self.state = VmmState::VmmServerReady;

            self.vmm_instance.shutdown_vm_instance().await.context("stop")?;
        }

        self.vmm_instance.stop()?;

        self.state = VmmState::NotReady;
    
        Ok(())
    }

    pub(crate) async fn pause_vm(&mut self) -> Result<()> {
        if self.state != VmmState::VmRunning {
            return Err(anyhow!(
                "cannot pause vm with VMM state {:?}",
                self.state
            ));
        }

        self.vmm_instance.vm_pause().await.context("pause vm")?;

        self.state = VmmState::VmPaused;

        Ok(())
    }

    pub(crate) async fn resume_vm(&mut self) -> Result<()> {
        if self.state != VmmState::VmPaused {
            return Err(anyhow!(
                "cannot resume vm with VMM state {:?}",
                self.state
            ));
        }

        self.vmm_instance.vm_resume().await.context("resume vm")?;

        self.state = VmmState::VmRunning;

        Ok(())
    }

    pub(crate) async fn save_vm(&mut self) -> Result<()> {
        if self.state != VmmState::VmPaused {
            return Err(anyhow!(
                "cannot save vm with VMM state {:?}",
                self.state
            ));
        }

        let snapshot_config = SnapshotConfig{
            destination_url: self.config.factory.template_path.clone(),
            ..Default::default()
        };

        self.vmm_instance.vm_snapshot(snapshot_config).await.context("save vm")?;

        Ok(())
    }

    pub(crate) async fn get_agent_socket(&self) -> Result<String> {
        const HYBRID_VSOCK_SCHEME: &str = "hvsock";

        let vsock_path = get_vsock_path(&self.id)?;

        let uri = format!("{HYBRID_VSOCK_SCHEME}://{vsock_path}");

        Ok(uri)
    }

    pub(crate) async fn disconnect(&mut self) {
        self.state = VmmState::NotReady;
    }

    pub(crate) async fn get_thread_ids(&self) -> Result<VcpuThreadIds> {
        let mut vcpu_thread_ids: VcpuThreadIds = VcpuThreadIds {
            vcpus: HashMap::new(),
        };

        for tid in self.vmm_instance.get_vcpu_tids() {
            vcpu_thread_ids.vcpus.insert(tid.0 as u32, tid.1);
        }
        info!(sl!(), "get thread ids {:?}", vcpu_thread_ids);
        Ok(vcpu_thread_ids)
    }

    pub(crate) async fn cleanup(&self) -> Result<()> {
        info!(sl!(), "CloudHypervisor::cleanup()");
        self.cleanup_resource();
        Ok(())
    }

    pub(crate) async fn get_pids(&self) -> Result<Vec<u32>> {
        let mut pids = HashSet::new();

        pids.insert(self.vmm_instance.pid());

        for tid in crate::ch::utils::get_child_threads(self.vmm_instance.pid()) {
            pids.insert(tid);
        }


        info!(sl!(), "get pids {:?}", pids);
        Ok(Vec::from_iter(pids.into_iter()))
    }

    pub(crate) async fn get_vmm_master_tid(&self) -> Result<u32> {
        let master_tid = self.vmm_instance.get_vmm_master_tid();
        Ok(master_tid)
    }

    pub(crate) async fn get_ns_path(&self) -> Result<String> {
        let ns_path = self.vmm_instance.get_ns_path();
        Ok(ns_path)
    }

    pub(crate) async fn check(&self) -> Result<()> {
        Ok(())
    }

    pub(crate) async fn get_jailer_root(&self) -> Result<String> {
        let root_path = get_jailer_root(&self.id);

        create_dir_all_with_inherit_owner(&root_path, 0o750)?;

        Ok(root_path)
    }

    pub(crate) async fn capabilities(&self) -> Result<Capabilities> {
        let mut caps = Capabilities::default();

        let flags = if guest_protection_is_tdx(self.guest_protection_to_use.clone()) {
            // TDX does not permit the use of virtio-fs.
            CapabilityBits::BlockDeviceSupport
                | CapabilityBits::BlockDeviceHotplugSupport
                | CapabilityBits::HybridVsockSupport
                | CapabilityBits::GuestMemoryProbe
        } else {
            CapabilityBits::BlockDeviceSupport
                | CapabilityBits::BlockDeviceHotplugSupport
                | CapabilityBits::FsSharingSupport
                | CapabilityBits::HybridVsockSupport
                | CapabilityBits::GuestMemoryProbe
        };

        caps.set(flags);

        Ok(caps)
    }

    pub(crate) async fn get_hypervisor_metrics(&self) -> Result<String> {
        Err(anyhow!("CH hypervisor metrics not implemented - see https://github.com/kata-containers/kata-containers/issues/8800"))
    }

    pub(crate) fn set_capabilities(&mut self, flag: CapabilityBits) {
        let mut caps = Capabilities::default();

        caps.set(flag)
    }

    pub(crate) fn set_guest_memory_block_size(&mut self, size: u32) {
        self.guest_memory_block_size_mb = size;
    }

    pub(crate) fn guest_memory_block_size_mb(&self) -> u32 {
        self.guest_memory_block_size_mb
    }

    pub(crate) async fn resize_memory(&mut self, new_mem_mb: u32) -> Result<(u32, MemoryConfig)> {
        if new_mem_mb > 5 * 1024 &&
            (self.config.memory_info.default_memory + self.mem_hotplug_size_mb < 1024 || self.config.vm_template.boot_from_template) {
            self.resize_memory_unit(5 * 1024).await?;
        }

        self.resize_memory_unit(new_mem_mb).await
    }

    pub(crate) async fn resize_memory_unit(&mut self, new_mem_mb: u32) -> Result<(u32, MemoryConfig)> {
         // check the invalid request memory
         if new_mem_mb > self.hypervisor_config().memory_info.default_maxmemory {
            warn!(
                sl!(),
                "memory size unchanged, the request memory size {} is greater than the max memory size {}",
                new_mem_mb, self.hypervisor_config().memory_info.default_maxmemory
            );

            return Ok((
                0,
                MemoryConfig {
                    ..Default::default()
                },
            ));
        }

        let default_memory = if self.config.vm_template.boot_from_template {
            self.config.vm_template.boot_from_template = false;
            VM_TEMPLATE_SIZE
        } else {
            self.config.memory_info.default_memory
        };
        let had_mem_mb = default_memory + self.mem_hotplug_size_mb;
        match new_mem_mb.cmp(&had_mem_mb) {
            Ordering::Equal => {
                // Everything is already set up
                info!(
                    sl!(),
                    "memory size unchanged, no need to do memory resizing"
                );
            }
            _ => {
                // update the hotplug size
                self.mem_hotplug_size_mb = if new_mem_mb > default_memory {
                    new_mem_mb - default_memory
                } else {
                    0
                };

                let vm_resize_data = VmResizeData{
                    desired_ram: Some(new_mem_mb as u64 * 1024 * 1024),
                    ..Default::default()
                };
                self.vmm_instance
                    .vm_resize(vm_resize_data)
                    .await
                    .context(format!(
                        "failed to do_resize_memory on new_memory={:?}",
                        new_mem_mb
                    ))?;
            }
        };

        Ok((new_mem_mb, MemoryConfig::default()))
    }

    // check if resizing info is valid
    // the error in this function is not ok to be tolerated, the container boot will fail
    fn precheck_resize_vcpus(&self, old_vcpus: u32, new_vcpus: u32) -> Result<(u32, u32)> {
        // old_vcpus > 0, safe for conversion
        let current_vcpus = old_vcpus;

        // a non-zero positive is required
        if new_vcpus == 0 {
            return Err(anyhow!("resize vcpu error: 0 vcpu resizing is invalid"));
        }

        // cannot exceed maximum value
        let default_maxvcpus = self.config.cpu_info.default_maxvcpus;
        if new_vcpus > default_maxvcpus {
            warn!(
                sl!(),
                "Cannot allocate more vcpus than the max allowed number of vcpus. The maximum allowed amount of vcpus will be used instead.");
            return Ok((current_vcpus, default_maxvcpus));
        }

        Ok((current_vcpus, new_vcpus))
    }

    pub(crate) async fn resize_vcpu(&mut self, old_vcpus: u32, new_vcpus: u32) -> Result<(u32, u32)> {
        if old_vcpus == new_vcpus {
            info!(
                sl!(),
                "resize_vcpu: no need to resize vcpus because old_vcpus is equal to new_vcpus"
            );
            return Ok((new_vcpus, new_vcpus));
        }

        let (old_vcpus, new_vcpus) = self.precheck_resize_vcpus(old_vcpus, new_vcpus)?;
        info!(
            sl!(),
            "check_resize_vcpus passed, passing new_vcpus = {:?} to vmm", new_vcpus
        );

        let vm_resize_data = VmResizeData{
            desired_vcpus: Some(new_vcpus.try_into().unwrap()),
            ..Default::default()
        };      
        self.vmm_instance
            .vm_resize(vm_resize_data)
            .await
            .context(format!(
                "failed to do_resize_vcpus on new_vcpus={:?}",
                new_vcpus
            ))?;

        self.config.cpu_info.current_vcpus = new_vcpus as f32;

        Ok((old_vcpus, new_vcpus))
    }

    /// Get the address of agent vsock server used to init connections for io
    pub(crate) async fn get_passfd_listener_addr(&self) -> Result<(String, u32)> {
        if let Some(passfd_port) = self.passfd_listener_port {
            let vsock_path = get_vsock_path(&self.id)?;
            Ok((vsock_path, passfd_port))
        } else {
            Err(anyhow!("passfd io listener port not set"))
        }
    }

    pub(crate) fn cleanup_resource(&self) {
        std::fs::remove_dir_all(&self.vm_path)
            .map_err(|err| {
                error!(sl!(), "failed to remove dir all for {}", &self.vm_path);
                err
            })
            .ok();
    }

    /// Get overlayfs block device
    pub(crate) async fn get_overlayfs_block_device(&self) -> Option<DeviceType> {
        self.overlayfs_block_device.clone()
    }
}

lazy_static! {
    // Store the fake guest protection value used by
    // get_fake_guest_protection() and set_fake_guest_protection().
    //
    // Note that if this variable is set to None, get_fake_guest_protection()
    // will fall back to checking the actual guest protection by calling
    // get_guest_protection().
    static ref FAKE_GUEST_PROTECTION: Arc<RwLock<Option<GuestProtection>>> =
        Arc::new(RwLock::new(Some(GuestProtection::NoProtection)));
}

// Return the _fake_ GuestProtection value set by set_guest_protection().
fn get_fake_guest_protection() -> Result<GuestProtection> {
    let existing_ref = FAKE_GUEST_PROTECTION.clone();

    let existing = existing_ref.read().unwrap();

    let real_protection = available_guest_protection()?;

    let protection = if let Some(ref protection) = *existing {
        protection
    } else {
        // XXX: If no fake value is set, fall back to the real function.
        &real_protection
    };

    Ok(protection.clone())
}

// Return available hardware protection, or GuestProtection::NoProtection
// if none available.
//
// XXX: Note that this function wraps the low-level function to determine
// guest protection. It does this to allow us to force a particular guest
// protection type in the unit tests.
fn get_guest_protection() -> Result<GuestProtection> {
    let guest_protection = if cfg!(test) {
        get_fake_guest_protection()
    } else {
        available_guest_protection().map_err(|e| anyhow!(e.to_string()))
    }?;

    Ok(guest_protection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kata_sys_util::protection::SevSnpDetails;

    #[cfg(target_arch = "x86_64")]
    use kata_sys_util::protection::TDX_KVM_PARAMETER_PATH;

    use kata_types::config::hypervisor::{Hypervisor as HypervisorConfig, SecurityInfo};
    use serial_test::serial;
    use test_utils::{assert_result, skip_if_not_root};

    use std::fs::File;
    use tempfile::Builder;

    fn set_fake_guest_protection(protection: Option<GuestProtection>) {
        let existing_ref = FAKE_GUEST_PROTECTION.clone();

        let mut existing = existing_ref.write().unwrap();

        // Modify the lazy static global config structure
        *existing = protection;
    }

    #[serial]
    #[actix_rt::test]
    async fn test_get_guest_protection() {
        // available_guest_protection() requires super user privs.
        skip_if_not_root!();

        let sev_snp_details = SevSnpDetails {
            cbitpos: 42,
            phys_addr_reduction: 42,
        };

        #[derive(Debug)]
        struct TestData {
            value: Option<GuestProtection>,
            result: Result<GuestProtection>,
        }

        let tests = &[
            TestData {
                value: Some(GuestProtection::NoProtection),
                result: Ok(GuestProtection::NoProtection),
            },
            TestData {
                value: Some(GuestProtection::Pef),
                result: Ok(GuestProtection::Pef),
            },
            TestData {
                value: Some(GuestProtection::Se),
                result: Ok(GuestProtection::Se),
            },
            TestData {
                value: Some(GuestProtection::Sev(sev_snp_details.clone())),
                result: Ok(GuestProtection::Sev(sev_snp_details.clone())),
            },
            TestData {
                value: Some(GuestProtection::Snp(sev_snp_details.clone())),
                result: Ok(GuestProtection::Snp(sev_snp_details.clone())),
            },
            TestData {
                value: Some(GuestProtection::Tdx),
                result: Ok(GuestProtection::Tdx),
            },
        ];

        for (i, d) in tests.iter().enumerate() {
            let msg = format!("test[{i}]: {d:?}");

            set_fake_guest_protection(d.value.clone());

            let result =
                task::spawn_blocking(|| -> Result<GuestProtection> { get_guest_protection() })
                    .await
                    .unwrap();

            let msg = format!("{msg}: actual result: {result:?}");

            if std::env::var("DEBUG").is_ok() {
                eprintln!("DEBUG: {msg}");
            }

            assert_result!(d.result, result, msg);
        }

        // Reset
        set_fake_guest_protection(None);
    }

    #[cfg(target_arch = "x86_64")]
    #[serial]
    #[actix_rt::test]
    async fn test_get_guest_protection_tdx() {
        // available_guest_protection() requires super user privs.
        skip_if_not_root!();

        // Use the hosts protection, not a fake one.
        set_fake_guest_protection(None);

        let have_tdx = fs::read(TDX_KVM_PARAMETER_PATH)
            .is_ok_and(|content| !content.is_empty() && content[0] == b'Y');

        let protection =
            task::spawn_blocking(|| -> Result<GuestProtection> { get_guest_protection() })
                .await
                .unwrap()
                .unwrap();

        if std::env::var("DEBUG").is_ok() {
            let msg = format!("have_tdx: {have_tdx:?}, protection: {protection:?}");

            eprintln!("DEBUG: {msg}");
        }

        if have_tdx {
            assert_eq!(protection, GuestProtection::Tdx);
        } else {
            assert_eq!(protection, GuestProtection::NoProtection);
        }
    }

    #[serial]
    #[actix_rt::test]
    async fn test_handle_guest_protection() {
        // available_guest_protection() requires super user privs.
        skip_if_not_root!();

        #[derive(Debug)]
        struct TestData {
            confidential_guest: bool,
            available_protection: Option<GuestProtection>,

            result: Result<()>,

            // The expected result (internal state)
            guest_protection_to_use: GuestProtection,
        }

        let tests = &[
            TestData {
                confidential_guest: false,
                available_protection: Some(GuestProtection::NoProtection),
                result: Ok(()),
                guest_protection_to_use: GuestProtection::NoProtection,
            },
            TestData {
                confidential_guest: true,
                available_protection: Some(GuestProtection::NoProtection),
                result: Err(anyhow!(GuestProtectionError::NoProtectionAvailable)),
                guest_protection_to_use: GuestProtection::NoProtection,
            },
            TestData {
                confidential_guest: false,
                available_protection: Some(GuestProtection::Tdx),
                result: Err(anyhow!(GuestProtectionError::TDXProtectionMustBeUsedWithCH)),
                guest_protection_to_use: GuestProtection::Tdx,
            },
            TestData {
                confidential_guest: true,
                available_protection: Some(GuestProtection::Tdx),
                result: Ok(()),
                guest_protection_to_use: GuestProtection::Tdx,
            },
            TestData {
                confidential_guest: false,
                available_protection: Some(GuestProtection::Pef),
                result: Ok(()),
                guest_protection_to_use: GuestProtection::NoProtection,
            },
            TestData {
                confidential_guest: true,
                available_protection: Some(GuestProtection::Pef),
                result: Err(anyhow!(GuestProtectionError::ExpectedTDXProtection(
                    GuestProtection::Pef
                ))),
                guest_protection_to_use: GuestProtection::Pef,
            },
        ];

        for (i, d) in tests.iter().enumerate() {
            let msg = format!("test[{i}]: {d:?}");

            set_fake_guest_protection(d.available_protection.clone());

            let mut ch = CloudHypervisorInner::default();

            let cfg = HypervisorConfig {
                security_info: SecurityInfo {
                    confidential_guest: d.confidential_guest,

                    ..Default::default()
                },

                ..Default::default()
            };

            ch.set_hypervisor_config(cfg);

            let result = ch.handle_guest_protection().await;

            let msg = format!("{msg}: actual result: {result:?}");

            if std::env::var("DEBUG").is_ok() {
                eprintln!("DEBUG: {msg}");
            }

            if d.result.is_ok() && result.is_ok() {
                continue;
            }

            assert_result!(d.result, result, msg);

            assert_eq!(
                ch.guest_protection_to_use, d.guest_protection_to_use,
                "{msg}"
            );
        }

        // Reset
        set_fake_guest_protection(None);
    }

    #[actix_rt::test]
    async fn test_get_kernel_params() {
        #[derive(Debug)]
        struct TestData<'a> {
            cfg: Option<HypervisorConfig>,
            confidential_guest: bool,
            debug: bool,
            fails: bool,
            contains: Vec<&'a str>,
        }

        let tests = &[
            TestData {
                cfg: None,
                confidential_guest: false,
                debug: false,
                fails: true, // No hypervisor config
                contains: vec![],
            },
            TestData {
                cfg: Some(HypervisorConfig::default()),
                confidential_guest: false,
                debug: false,
                fails: false,
                contains: vec![],
            },
        ];

        for (i, d) in tests.iter().enumerate() {
            let msg = format!("test[{i}]: {d:?}");

            let mut ch = CloudHypervisorInner::default();

            if let Some(ref mut cfg) = d.cfg.clone() {
                if d.debug {
                    cfg.debug_info.enable_debug = true;
                }

                if d.confidential_guest {
                    cfg.security_info.confidential_guest = true;
                }

                ch.set_hypervisor_config(cfg.clone());

                let result = ch.get_kernel_params().await;

                let msg = format!("{msg}: actual result: {result:?}");

                if std::env::var("DEBUG").is_ok() {
                    eprintln!("DEBUG: {msg}");
                }

                if d.fails {
                    assert!(result.is_err(), "{}", msg);
                    continue;
                }

                let result = result.unwrap();

                for token in d.contains.clone() {
                    assert!(result.contains(token), "{}", msg);
                }
            }
        }
    }
}

