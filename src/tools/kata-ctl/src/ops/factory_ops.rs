// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::sync::Arc;
use std::result::Result::Ok;
use tokio::runtime::Runtime;

use crate::args::{FactoryArgs, FactorySubCommand};
use anyhow::{anyhow, Context, Result};
use kata_types::config::TomlConfig;
use virt_container::{
    factory::{factory::VMFactory, get_factory_base, FactoryBase,
         template::TemplateError,
    },
    VirtContainer,
};
use common::RuntimeHandler;
use kata_types::config::hypervisor::HYPERVISOR_NAME_CH;

macro_rules! sl {
    () => {
        slog_scope::logger().new(o!("subsystem" => "factory_ops"))
    };
}

pub fn handle_factory(factory_args: FactoryArgs) -> Result<()> {
    let rt = Runtime::new().context("failed to create Tokio runtime")?;
    rt.block_on(async {
        match &factory_args.command {
            FactorySubCommand::Init => {
                init()
                    .await
                    .context("failed to initialize factory")?;
            }
            FactorySubCommand::Destroy => {
                destroy()
                    .await
                    .context("failed to destroy factory")?;
            }
            FactorySubCommand::Status => {
                status()
                    .await
                    .context("failed to query factory status")?;
            }
        }
        Ok(())
    })
}

fn update_agent_kernel_params(config: &mut TomlConfig) -> Result<()> {
    let mut params = vec![];
    if let Ok(kv) = config.get_agent_kernel_params() {
        for (k, v) in kv.into_iter() {
            if let Ok(s) = kata_hypervisor::Param::new(k.as_str(), v.as_str()).to_string() {
                params.push(s);
            }
        }
        if let Some(h) = config.hypervisor.get_mut(&config.runtime.hypervisor_name) {
            h.boot_info.add_kernel_params(params);
        }
    }
    Ok(())
}

fn load_config() -> Result<Arc<TomlConfig>> {
    VirtContainer::init().context("init virt container")?;
    slog::info!(sl!(), "Load factory config");

    let (mut toml_config, _) = TomlConfig::load_from_file(&String::from("")).context(format!(
        "failed to load TOML config (tried {:?})",
        TomlConfig::get_default_config_file_list()
    ))?;

    update_agent_kernel_params(&mut toml_config)?;

    toml_config.validate()?;

    let factory_cfg = toml_config.get_factory();
    if factory_cfg.factory_type != "template" {
        return Err(anyhow!("template not enabled, factory_type is {}", factory_cfg.factory_type));
    }

    let hypervisor_name = &toml_config.runtime.hypervisor_name;

    match hypervisor_name.as_str() {
        HYPERVISOR_NAME_CH => {
            let hypervisor = toml_config
                        .hypervisor
                        .get_mut(hypervisor_name)
                        .ok_or_else(|| anyhow!("failed to get hypervisor for {}", hypervisor_name))?;

            hypervisor.vm_template.boot_from_template = false;
            hypervisor.vm_template.boot_to_be_template = true;

            Ok(Arc::new(toml_config))
        }
        kata_hypervisor::HYPERVISOR_QEMU => {
            let hypervisor = toml_config
                        .hypervisor
                        .get_mut(hypervisor_name)
                        .ok_or_else(|| anyhow!("failed to get hypervisor for {}", hypervisor_name))?;

            hypervisor.vm_template.boot_from_template = false;
            hypervisor.vm_template.boot_to_be_template = true;
            let path = std::path::Path::new(&factory_cfg.template_path);
            hypervisor.vm_template.memory_path = path.join("memory").to_string_lossy().to_string();
            hypervisor.vm_template.device_state_path = path.join("state").to_string_lossy().to_string();

            Ok(Arc::new(toml_config))
        }
        _ => Err(anyhow!("unsupported hypervisor: {}", hypervisor_name))
    }
}

async fn init() -> Result<()> {
    let config = load_config()?;

    match get_factory_base(config.clone(), true).await {
        Err(e) => match e.downcast_ref::<TemplateError>() {
            Some(TemplateError::FileExists) => {
                slog::error!(sl!(), "vm factory already exists");
                return Ok(());
            }
            _ => (),
        },
        _ => (),
    }

    match get_factory_base(config.clone(), false).await {
        Ok(_) => {
            slog::info!(sl!(), "create vm factory successfully");
            Ok(())
        },
        Err(e) => Err(e.into()),
    }
}

async fn destroy() -> Result<()> {
    let config = load_config()?;

    match get_factory_base(config.clone(), true).await {
        Err(e) => match e.downcast_ref::<TemplateError>() {
            Some(TemplateError::FileNotExists) => {
                slog::info!(sl!(), "vm factory not exists");
                return Ok(());
            }
            _ => (),
        },
        _ => (),
    }

    let factory_impl = match get_factory_base(config.clone(), false).await {
        Ok(factory_impl) => factory_impl,
        Err(e) => return Err(e.into()),
    };
    let factory = VMFactory::new(factory_impl);

    factory.close_factory().await?;

    slog::info!(sl!(), "vm factory destroyed");

    Ok(())
}

async fn status() -> Result<()> {
    let config = load_config()?;

    match get_factory_base(config.clone(), true).await {
        Ok(_) => {
            slog::info!(sl!(), "vm factory is on");
            Ok(())
        },
        Err(e) => match e.downcast_ref::<TemplateError>() {
            Some(TemplateError::FileNotExists) => {
                slog::info!(sl!(), "vm factory is off");
                Ok(())
            }
            Some(TemplateError::FileExists) => {
                slog::info!(sl!(), "vm factory is on");
                Ok(())
            }
            _ => Err(e.into()),
        },
    }
}
