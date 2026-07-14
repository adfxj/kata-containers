// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::{new_agent, new_hypervisor};
use super::{
    vm::{BareVM, VMConfig},
    FactoryBase,
};

#[derive(Debug)]
pub struct Direct {
    config: Arc<VMConfig>,
}

impl Direct {
    pub fn new(config: Arc<VMConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl FactoryBase for Direct {
    fn config(&self) -> Arc<VMConfig> {
        self.config.clone()
    }

    async fn get_base_vm(&self, config: Arc<VMConfig>) -> Result<BareVM> {
        let hypervisor = new_hypervisor(config.as_ref().config.as_ref())
            .await
            .context("new hypervisor")?;

        let agent = new_agent(config.as_ref().config.as_ref()).context("new agent")?;

        Ok(BareVM::new(hypervisor, agent))
    }

    async fn close_factory(&self) -> Result<()> {
        Ok(())
    }
}
