// Copyright 2025 Kata Contributors
// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::sync::Arc;

use agent::{kata::KataAgent, Agent, AGENT_KATA};
use anyhow::{anyhow, Context, Result};
use kata_hypervisor::device::driver::{VIRTIO_BLOCK_CCW, VIRTIO_BLOCK_PCI};
use kata_hypervisor::{qemu::Qemu, Hypervisor, HYPERVISOR_QEMU};
use kata_types::config::{
    default, Agent as AgentConfig, Hypervisor as HypervisorConfig, TomlConfig,
};
use kata_types::machine_type::MACHINE_TYPE_S390X_TYPE;
use serde::{Deserialize, Serialize};

#[cfg(all(
    feature = "cloud-hypervisor",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use kata_hypervisor::ch::CloudHypervisor;
#[cfg(all(
    feature = "cloud-hypervisor",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use kata_types::config::hypervisor::HYPERVISOR_NAME_CH;

pub struct BareVM {
    hypervisor: Arc<dyn Hypervisor>,
    agent: Arc<dyn Agent>,
}

impl BareVM {
    pub fn new(hypervisor: Arc<dyn Hypervisor>, agent: Arc<dyn Agent>) -> Self {
        Self { hypervisor, agent }
    }

    pub fn get_hypervisor(&self) -> Arc<dyn Hypervisor> {
        self.hypervisor.clone()
    }

    pub fn get_agent(&self) -> Arc<dyn Agent> {
        self.agent.clone()
    }

    pub async fn ncpus(&self) -> f32 {
        self.hypervisor
            .hypervisor_config()
            .await
            .cpu_info
            .default_vcpus
    }

    pub async fn mem_size(&self) -> u32 {
        self.hypervisor
            .hypervisor_config()
            .await
            .memory_info
            .default_memory
    }
}

#[derive(Debug)]
pub struct VMConfig {
    pub config: Arc<TomlConfig>,
}

impl VMConfig {
    pub fn new(config: Arc<TomlConfig>) -> Self {
        Self { config }
    }

    pub fn hypervisor_name(&self) -> String {
        self.config.runtime.hypervisor_name.clone()
    }

    pub fn agent_name(&self) -> String {
        self.config.runtime.agent_name.clone()
    }

    pub fn hypervisor_config(&self) -> Result<&HypervisorConfig> {
        let hypervisor_name = self.hypervisor_name();
        let hypervisor_config = self
            .config
            .hypervisor
            .get(&hypervisor_name)
            .ok_or_else(|| anyhow!("failed to get hypervisor for {}", &hypervisor_name))?;
        Ok(hypervisor_config)
    }

    pub fn agent_config(&self) -> Result<&AgentConfig> {
        let agent_name = self.agent_name();
        let agent_config = self
            .config
            .agent
            .get(&agent_name)
            .ok_or_else(|| anyhow!("failed to get agent for {}", &agent_name))?;
        Ok(agent_config)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VmConfig {
    #[serde(default)]
    pub hypervisor_name: String,
    #[serde(default)]
    pub agent_name: String,
    #[serde(default)]
    pub agent_config: AgentConfig,
    #[serde(default)]
    pub hypervisor_config: HypervisorConfig,
}

impl VmConfig {
    pub fn new(toml_config: &TomlConfig) -> Self {
        let hypervisor_name = toml_config.runtime.hypervisor_name.clone();
        let agent_name = toml_config.runtime.agent_name.clone();

        let hypervisor_config = toml_config
            .hypervisor
            .get(&hypervisor_name)
            .cloned()
            .unwrap_or_default();

        let agent_config = toml_config
            .agent
            .get(&agent_name)
            .cloned()
            .unwrap_or_default();

        VmConfig {
            hypervisor_name,
            agent_name,
            hypervisor_config,
            agent_config,
        }
    }

    fn validate_boot_configuration(conf: &HypervisorConfig) -> Result<()> {
        let is_secure_execution = conf.security_info.confidential_guest
            && conf.machine_info.machine_type == MACHINE_TYPE_S390X_TYPE;

        let has_image = !conf.boot_info.image.is_empty();
        let has_initrd = !conf.boot_info.initrd.is_empty();

        if is_secure_execution {
            if has_image || has_initrd {
                return Err(anyhow!(
                    "secure execution mode does not allow image or initrd"
                ));
            }
            return Ok(());
        }

        if !has_image && !has_initrd {
            return Err(anyhow!("missing image and initrd path"));
        }

        if has_image && has_initrd {
            return Err(anyhow!("image and initrd path cannot both be set"));
        }

        Ok(())
    }

    pub fn validate_hypervisor_config(conf: &mut HypervisorConfig) -> Result<()> {
        if !conf.remote_info.hypervisor_socket.is_empty() {
            return Ok(());
        }

        if conf.boot_info.kernel.is_empty() {
            return Err(anyhow!("missing kernel path"));
        }

        Self::validate_boot_configuration(conf)?;

        if conf.cpu_info.default_vcpus == 0.0 {
            conf.cpu_info.default_vcpus = default::DEFAULT_GUEST_VCPUS as f32;
        }

        if conf.memory_info.default_memory == 0 {
            conf.memory_info.default_memory = default::DEFAULT_QEMU_MEMORY_SIZE_MB;
        }

        if conf.device_info.default_bridges == 0 {
            conf.device_info.default_bridges = default::DEFAULT_QEMU_PCI_BRIDGES;
        }

        if conf.blockdev_info.block_device_driver.is_empty() {
            conf.blockdev_info.block_device_driver = default::DEFAULT_BLOCK_DEVICE_TYPE.to_string();
        } else if conf.blockdev_info.block_device_driver == VIRTIO_BLOCK_PCI
            && conf.machine_info.machine_type == MACHINE_TYPE_S390X_TYPE
        {
            conf.blockdev_info.block_device_driver = VIRTIO_BLOCK_CCW.to_string();
        }

        if conf.cpu_info.default_maxvcpus == 0
            || conf.cpu_info.default_maxvcpus > default::MAX_QEMU_VCPUS
        {
            conf.cpu_info.default_maxvcpus = default::MAX_QEMU_VCPUS;
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct TemplateVm {
    pub hypervisor: Arc<dyn Hypervisor>,
    pub agent: Arc<dyn Agent>,
    pub id: String,
    pub cpu: f32,
    pub memory: u32,
    pub cpu_delta: i32,
}

impl TemplateVm {
    pub fn new(
        id: String,
        hypervisor: Arc<dyn Hypervisor>,
        agent: Arc<dyn Agent>,
        cpu: f32,
        memory: u32,
    ) -> Self {
        Self {
            id,
            hypervisor,
            agent,
            cpu,
            memory,
            cpu_delta: 0,
        }
    }

    async fn new_hypervisor(config: &VmConfig) -> Result<Arc<dyn Hypervisor>> {
        let hypervisor: Arc<dyn Hypervisor> = match config.hypervisor_name.as_str() {
            HYPERVISOR_QEMU => {
                let h = Qemu::new();
                h.set_hypervisor_config(config.hypervisor_config.clone())
                    .await;
                Arc::new(h)
            }
            HYPERVISOR_NAME_CH => {
                let mut h = CloudHypervisor::new();
                h.set_hypervisor_config(config.hypervisor_config.clone())
                    .await;
                Arc::new(h)
            }
            _ => return Err(anyhow!("Unsupported hypervisor {}", config.hypervisor_name)),
        };
        Ok(hypervisor)
    }

    fn new_agent(config: &VmConfig) -> Result<Arc<KataAgent>> {
        let agent_name = &config.agent_name;
        let agent_config = config.agent_config.clone();

        match agent_name.as_str() {
            AGENT_KATA => {
                let agent = KataAgent::new(agent_config.clone());
                Ok(Arc::new(agent))
            }
            _ => Err(anyhow!("Unsupported agent {}", &agent_name)),
        }
    }

    pub async fn new_vm(config: VmConfig, toml_config: TomlConfig) -> Result<Self> {
        use common::{message::Message, types::SandboxConfig, Sandbox, SandboxNetworkEnv};
        use resource::{cpu_mem::initial_size::InitialSizeManager, ResourceManager};
        use runtime_spec;
        use std::collections::HashMap;
        use tokio::sync::mpsc::channel;
        use uuid::Uuid;

        const MESSAGE_BUFFER_SIZE: usize = 8;

        let sid = Uuid::new_v4().to_string();

        let (sender, _receiver) = channel::<Message>(MESSAGE_BUFFER_SIZE);

        let hypervisor = Self::new_hypervisor(&config)
            .await
            .context("new hypervisor")?;

        let agent = Self::new_agent(&config).context("new agent")?;

        let sandbox_config = SandboxConfig {
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
        };

        let initial_size_manager = InitialSizeManager::new_from(&sandbox_config.annotations)
            .context("failed to construct static resource manager")?;

        let factory = toml_config.get_factory();

        let toml_config_arc = Arc::new(toml_config);

        let resource_manager = Arc::new(
            ResourceManager::new(
                &sid,
                agent.clone(),
                hypervisor.clone(),
                toml_config_arc,
                initial_size_manager,
            )
            .await
            .context("build resource manager")?,
        );

        let sandbox = crate::sandbox::VirtSandbox::new(
            &sid,
            sender.clone(),
            agent.clone(),
            hypervisor.clone(),
            resource_manager.clone(),
            sandbox_config,
            factory,
        )
        .await
        .context("build sandbox")?;

        sandbox.start_template().await.context("start template")?;
        info!(sl!(), "VM has been started from template");

        let hypervisor_config = sandbox.get_hypervisor().hypervisor_config().await;
        let vm = TemplateVm::new(
            sandbox.get_sid(),
            sandbox.get_hypervisor(),
            sandbox.get_agent(),
            hypervisor_config.cpu_info.default_vcpus as f32,
            hypervisor_config.memory_info.default_memory,
        );
        Ok(vm)
    }

    pub async fn stop(&self) -> Result<()> {
        self.hypervisor
            .stop_vm()
            .await
            .map_err(|e| anyhow::anyhow!("failed to stop vm: {}", e))
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.agent.disconnect().await.context("disconnect vm")
    }

    pub async fn pause(&self) -> Result<()> {
        self.hypervisor.pause_vm().await.context("pause vm")
    }

    pub async fn save(&self) -> Result<()> {
        self.hypervisor.save_vm().await.context("save vm")
    }

    pub async fn resume(&self) -> Result<()> {
        self.hypervisor.resume_vm().await.context("resume vm")
    }
}
