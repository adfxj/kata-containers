// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

pub mod factory;
pub mod template;
pub mod vm;
pub mod direct;

use std::sync::Arc;

use anyhow::{anyhow, Ok, Result};
use async_trait::async_trait;
use kata_types::config::hypervisor::HYPERVISOR_NAME_CH;
use kata_types::config::TomlConfig;
use template::Template;
use direct::Direct;

use self::vm::VMConfig;
use self::factory::VMFactory;

#[async_trait]
pub trait FactoryBase: std::fmt::Debug + Sync + Send {
    fn config(&self) -> Arc<vm::VMConfig>;
    async fn get_base_vm(&self, config: Arc<vm::VMConfig>) -> Result<vm::BareVM>;
    async fn close_factory(&self) -> Result<()>;
}

#[async_trait]
pub trait Factory: FactoryBase {
    async fn get_vm(&self, config: Arc<vm::VMConfig>) -> Result<vm::BareVM>;
}

pub async fn get_factory_instance(config: Arc<TomlConfig>, fetch_only: bool) -> Result<Arc<dyn Factory>> {
    let vm_config = Arc::new(VMConfig::new(config.clone()));
    let factory_base: Arc<dyn FactoryBase> = match vm_config.hypervisor_name().as_str() {
        HYPERVISOR_NAME_CH => {
            get_factory_base(config, fetch_only).await?
        }
        _ => {
            Arc::new(Direct::new(vm_config))
        }
    };
    Ok(Arc::new(VMFactory::new(factory_base)))
}

pub async fn get_factory_base(config: Arc<TomlConfig>, fetch_only: bool) -> Result<Arc<dyn FactoryBase>> {
    let vm_config = Arc::new(VMConfig::new(config.clone()));

    let factory = config.get_factory();
    let factory_type = factory.factory_type.as_str();

    info!(
        sl!(),
        "getting factory type {:?}", factory_type
    );

    match factory_type {
        "template" => {
            let template_path = factory.template_path.clone();
            if template_path.is_empty() {
                return Err(anyhow!("template_path is not set"));
            }
            let template = Template::new(template_path, vm_config, fetch_only).await?;
            let factory_base: Arc<dyn FactoryBase> = template;
            Ok(factory_base)
        }
        "cache" => {
            Err(anyhow!("cache factory is not supported yet"))
        }
        "direct" => {
            Ok(Arc::new(Direct::new(vm_config)))
        }
        _ => {
            Err(anyhow!("vm factory not enabled"))
        }
    }
}
