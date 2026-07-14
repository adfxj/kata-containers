// Copyright 2025 Kata Contributors
// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use agent::AgentManager;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use kata_hypervisor::VM_TEMPLATE_SIZE;
use kata_types::config::TomlConfig;
use kata_hypervisor::HYPERVISOR_QEMU;
use nix::mount::{mount, umount2, MsFlags};
use nix::sys::stat;
use scopeguard::defer;
use slog::{error, info};
use tokio::sync::mpsc::channel;

#[cfg(all(
    feature = "cloud-hypervisor",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use kata_types::config::hypervisor::HYPERVISOR_NAME_CH;

use common::{
    message::Message,
    types::SandboxConfig,
    SandboxNetworkEnv,
};
use resource::cpu_mem::initial_size::InitialSizeManager;
use resource::ResourceManager;
use oci_spec::runtime as oci;
use runtime_spec;
use uuid::Uuid;

use crate::factory::vm::{BareVM, VmConfig, VMConfig, TemplateVm};
use crate::factory::FactoryBase;
use crate::sandbox::VirtSandbox;

const TEMPLATE_DEVICE_STATE_SIZE: u32 = 8;
const TEMPLATE_WAIT_FOR_AGENT: Duration = Duration::from_secs(2);
const MESSAGE_BUFFER_SIZE: usize = 8;

fn default_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        sandbox_id: String::new(),
        hostname: String::new(),
        dns: Vec::new(),
        network_env: SandboxNetworkEnv::default(),
        annotations: HashMap::default(),
        hooks: None,
        state: runtime_spec::State {
            version: Default::default(),
            id: String::new(),
            status: runtime_spec::ContainerState::Creating,
            pid: 0,
            bundle: String::new(),
            annotations: Default::default(),
        },
        shm_size: 0,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("template file exists.")]
    FileExists,
    #[error("template file not exists.")]
    FileNotExists,
    #[error("template is not supported by hypervisor type: {0}")]
    NotSupported(String),
    #[error("template create failed")]
    CreateFailed,
    #[error("template prepare failed")]
    PrepareFailed,
}

fn is_mounted(path: &Path) -> Result<bool> {
    let mounts = fs::read_to_string("/proc/mounts")
        .context("Failed to read /proc/mounts")?;
    Ok(mounts.lines().any(|line| {
        line.split_whitespace().nth(1) == Some(path.to_str().unwrap_or(""))
    }))
}

#[derive(Debug)]
pub struct Template {
    template_path: String,
    config: Arc<VMConfig>,
}

impl Template {
    pub async fn new(template_path: String, config: Arc<VMConfig>, fetch_only: bool) -> Result<Arc<Template>> {
        let template = Self {
            template_path: template_path.clone(),
            config,
        };

        let hypervisor_name = template.config.hypervisor_name();

        match hypervisor_name.as_str() {
            HYPERVISOR_NAME_CH => {
                if template.check_clh_template_vm() {
                    if fetch_only {
                        return Err(TemplateError::FileExists.into());
                    } else {
                        return Ok(Arc::new(template));
                    }
                } else {
                    if fetch_only {
                        return Err(TemplateError::FileNotExists.into());
                    }
                }

                template.prepare_clh_template_files().map_err(|error| {
                    error!(sl!(), "prepare template error: {:?}", error);
                    let _ = template.close();
                    TemplateError::PrepareFailed
                })?;

                template.create_template_vm()
                    .await
                    .map_err(|error| {
                    error!(sl!(), "create template error: {:?}", error);
                    let _ = template.close();
                    TemplateError::CreateFailed
                })?;
            }
            HYPERVISOR_QEMU => {
                if template.check_qemu_template_vm() {
                    if fetch_only {
                        return Err(TemplateError::FileExists.into());
                    } else {
                        return Ok(Arc::new(template));
                    }
                } else {
                    if fetch_only {
                        return Err(TemplateError::FileNotExists.into());
                    }
                }

                template.prepare_qemu_template_files().map_err(|error| {
                    error!(sl!(), "prepare template error: {:?}", error);
                    TemplateError::PrepareFailed
                })?;

                template.create_qemu_template_vm()
                    .await
                    .map_err(|error| {
                    error!(sl!(), "create template error: {:?}", error);
                    TemplateError::CreateFailed
                })?;
            }
            _ => return Err(TemplateError::NotSupported(hypervisor_name).into()),
        }

        Ok(Arc::new(template))
    }

    fn check_clh_template_vm(&self) -> bool {
        let base_path = Path::new(&self.template_path);
        let files = ["memory-ranges", "state.json", "config.json"];
        for file in &files {
            let file_path = base_path.join(file);
            if !file_path.exists() {
                return false;
            }
        }
        true
    }

    fn check_qemu_template_vm(&self) -> bool {
        let memory_path = Path::new(&self.template_path).join("memory");
        let state_path = Path::new(&self.template_path).join("state");
        memory_path.exists() && state_path.exists()
    }

    fn prepare_clh_template_files(&self) -> Result<()> {
        let template_path = Path::new(&self.template_path);
        if is_mounted(template_path)? {
            return Ok(())
        }
        fs::create_dir_all(template_path)
            .context(format!("Failed to create template directory {:?}", template_path))?;

        let need_cleanup = Cell::new(true);
        defer! {
            if need_cleanup.get() {
                let _ = fs::remove_dir_all(template_path)
                    .context(format!("Failed to remove template directory: {:?}", template_path));
            }
        }

        let c_path = CString::new(template_path.to_str().ok_or_else(||
            anyhow!("Invalid template path"))?)?;

        unsafe {
            if libc::chmod(c_path.as_ptr(), stat::Mode::S_IRWXU.bits()) != 0 {
                return Err(anyhow!("Failed to set directory permissions: {}",
                    std::io::Error::last_os_error()));
            }
        }

        let memory_size = VM_TEMPLATE_SIZE + TEMPLATE_DEVICE_STATE_SIZE;
        let mount_options = format!("size={}M", memory_size);
        let fs_type = CString::new("tmpfs").context("Failed to create CString for tmpfs")?;
        let data = CString::new(mount_options).context("Failed to create CString for mount options")?;
        mount(
            Some(fs_type.as_c_str()),
            template_path,
            Some(fs_type.as_c_str()),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(data.as_c_str()),
        ).context("Failed to mount tmpfs")?;

        need_cleanup.set(false);
        Ok(())
    }

    fn prepare_qemu_template_files(&self) -> Result<()> {
        let state_path = Path::new(&self.template_path);

        std::fs::create_dir_all(state_path)
            .context(format!("failed to create directory: {:?}", state_path))?;

        if !state_path.exists() {
            return Err(anyhow!(
                "state path {:?} does not exist after creation",
                state_path
            ));
        }

        let opts = format!(
            "size={}M",
            self.config.hypervisor_config().unwrap().memory_info.default_memory
                + TEMPLATE_DEVICE_STATE_SIZE
        );
        mount(
            Some("tmpfs"),
            state_path,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some(opts.as_str()),
        ).context(format!("failed to mount tmpfs at {:?}", state_path))?;

        if !state_path.is_dir() {
            return Err(anyhow!(
                "state path {:?} is not a directory after mount",
                state_path
            ));
        }

        let memory_file = state_path.join("memory");
        std::fs::File::create(&memory_file)
            .context(format!("failed to create memory file: {memory_file:?}"))?;

        if !memory_file.exists() {
            return Err(anyhow!(
                "memory file {:?} does not exist after creation",
                memory_file
            ));
        }

        Ok(())
    }

    async fn create_template_vm(&self) -> Result<()> {
        let config = self.create_to_template_clh()?;

        let hypervisor = crate::new_hypervisor(&config).await.context("new hypervisor")?;
        let agent = crate::new_agent(&config).context("new agent")?;

        let id = Uuid::new_v4().to_string();
        let spec = oci::Spec::default();
        let initial_size_manager = InitialSizeManager::new(&spec).context("failed to construct static resource manager")?;
        let empty_anno_map: HashMap<String, String> = HashMap::new();

        let network_env = SandboxNetworkEnv {
            netns: None,
            network_created: false,
            annotations: HashMap::new(),
        };

        let resource_manager = Arc::new(
            ResourceManager::new(
                &id,
                agent.clone(),
                hypervisor.clone(),
                Arc::new(config.clone()),
                initial_size_manager,
            )
            .await?,
        );

        let (sender, _receiver) = channel::<Message>(MESSAGE_BUFFER_SIZE);

        let sandbox = VirtSandbox::new(
            &id,
            sender,
            agent.clone(),
            hypervisor.clone(),
            resource_manager.clone(),
            default_sandbox_config(),
            config.get_factory(),
        )
        .await
        .context("new virt sandbox")?;

        let result: Result<()> = async {
            hypervisor.prepare_vm(&id, network_env.netns.clone(), &empty_anno_map, None)
                .await
                .context("prepare vm")?;

            let resources = sandbox
                .prepare_for_start_sandbox(&id, &default_sandbox_config())
                .await?;

            resource_manager.prepare_before_start_vm(resources)
                .await
                .context("set up device before start vm")?;

            hypervisor.start_vm(10_000).await.context("start vm")?;
            info!(sl!(), "start vm");

            let address = hypervisor
                .get_agent_socket()
                .await
                .context("get agent socket")?;
            agent.start_without_log_forwarder(&address).await.context("connect")?;

            resource_manager
                .setup_after_start_vm()
                .await
                .context("setup device after start vm")?;

            sleep(TEMPLATE_WAIT_FOR_AGENT);

            hypervisor.pause_vm().await?;
            hypervisor.save_vm().await?;
            hypervisor.stop_vm().await?;
            hypervisor.cleanup().await?;

            Ok(())
        }
        .await;

        resource_manager.cleanup().await?;
        result
    }

    async fn create_qemu_template_vm(&self) -> Result<()> {
        let mut vm_config = self.config.config.as_ref().clone();
        let hypervisor_name = &vm_config.runtime.hypervisor_name;
        if let Some(h) = vm_config.hypervisor.get_mut(hypervisor_name) {
            h.vm_template.boot_to_be_template = true;
            h.vm_template.boot_from_template = false;
            h.vm_template.memory_path =
                Path::new(&self.template_path).join("memory").to_string_lossy().to_string();
            h.vm_template.device_state_path =
                Path::new(&self.template_path).join("state").to_string_lossy().to_string();
        } else {
            return Err(anyhow!("hypervisor '{}' not found", hypervisor_name));
        }

        let config = VmConfig::new(&vm_config);
        let vm = TemplateVm::new_vm(config, vm_config)
            .await
            .context("new template vm")?;

        vm.disconnect().await.context("disconnect template vm")?;

        sleep(TEMPLATE_WAIT_FOR_AGENT);

        vm.pause().await.context("pause template vm")?;
        vm.save().await.context("save template vm")?;

        Ok(())
    }

    async fn create_from_template(&self) -> Result<BareVM> {
        let hypervisor_name = self.config.hypervisor_name();

        match hypervisor_name.as_str() {
            HYPERVISOR_NAME_CH => self.create_from_clh_template().await,
            HYPERVISOR_QEMU => self.create_from_qemu_template().await,
            _ => Err(TemplateError::NotSupported(hypervisor_name).into()),
        }
    }

    async fn create_from_clh_template(&self) -> Result<BareVM> {
        let mut config = self.config.config.as_ref().clone();
        let hypervisor_name = &config.runtime.hypervisor_name;
        if let Some(h) = config.hypervisor.get_mut(hypervisor_name) {
            h.vm_template.boot_from_template = true;
            h.vm_template.boot_to_be_template = false;
            h.factory.template_path = format!("file://{}", self.template_path.clone());
        } else {
            return Err(anyhow!("hypervisor '{}' not found", hypervisor_name));
        }

        let hypervisor = crate::new_hypervisor(&config).await.context("new hypervisor")?;
        let agent = crate::new_agent(&config).context("new agent")?;

        Ok(BareVM::new(hypervisor, agent))
    }

    async fn create_from_qemu_template(&self) -> Result<BareVM> {
        let mut config = self.config.config.as_ref().clone();
        let hypervisor_name = &config.runtime.hypervisor_name;
        if let Some(h) = config.hypervisor.get_mut(hypervisor_name) {
            h.vm_template.boot_to_be_template = false;
            h.vm_template.boot_from_template = true;
            h.vm_template.memory_path =
                Path::new(&self.template_path).join("memory").to_string_lossy().to_string();
            h.vm_template.device_state_path =
                Path::new(&self.template_path).join("state").to_string_lossy().to_string();
        } else {
            return Err(anyhow!("hypervisor '{}' not found", hypervisor_name));
        }

        let hypervisor = crate::new_hypervisor(&config).await.context("new hypervisor")?;
        let agent = crate::new_agent(&config).context("new agent")?;

        Ok(BareVM::new(hypervisor, agent))
    }

    fn create_to_template_clh(&self) -> Result<TomlConfig> {
        let mut config = self.config.config.as_ref().clone();
        let hypervisor_name = &config.runtime.hypervisor_name;
        if let Some(h) = config.hypervisor.get_mut(hypervisor_name) {
            h.vm_template.boot_from_template = false;
            h.vm_template.boot_to_be_template = true;
            h.factory.template_path = format!("file://{}", self.template_path.clone());
        } else {
            return Err(anyhow!("hypervisor '{}' not found", hypervisor_name));
        }

        Ok(config.clone())
    }

    fn close(&self) -> Result<()> {
        let template_path = Path::new(&self.template_path);

        if !template_path.exists() {
            return Ok(())
        }

        umount2(template_path, nix::mount::MntFlags::MNT_DETACH)
            .context(format!("Failed to unmount template directory: {:?}", template_path))?;

        fs::remove_dir_all(template_path)
            .context(format!("Failed to remove template directory: {:?}", template_path))
    }
}

#[async_trait]
impl FactoryBase for Template {
    fn config(&self) -> Arc<VMConfig> {
        self.config.clone()
    }

    async fn get_base_vm(&self, _config: Arc<VMConfig>) -> Result<BareVM> {
        self.create_from_template().await
    }

    async fn close_factory(&self) -> Result<()> {
        self.close()
    }
}
