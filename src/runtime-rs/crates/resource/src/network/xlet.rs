#![allow(dead_code)]
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use kata_hypervisor::device::device_manager::DeviceManager;
use kata_hypervisor::Hypervisor;
use scopeguard::defer;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::network_entity::NetworkEntity;
use super::{EndpointState, Network};
use crate::network::endpoint::TapEndpoint;
use crate::network::network_info::network_info_from_xlet::NetworkInfoFromXlet;
use crate::network::utils::generate_private_mac_addr;
use crate::network::Endpoint;

pub struct Xlet {
    inner: Arc<RwLock<XletInner>>,
}

pub struct XletInner {
    entity_list: Vec<NetworkEntity>,
}

impl Xlet {
    pub async fn new(
        config: &XletNetworkConfig,
        dev_mgr: Arc<RwLock<DeviceManager>>,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(RwLock::new(XletInner::new(config, &dev_mgr).await?)),
        })
    }
}

impl XletInner {
    async fn new(config: &XletNetworkConfig, dev_mgr: &Arc<RwLock<DeviceManager>>) -> Result<Self> {
        // Create rtnetlink connection
        let (connection, handle, _) =
            rtnetlink::new_connection().context("Failed to create rtnetlink connection")?;
        let thread_handler = tokio::spawn(connection);
        defer!({
            thread_handler.abort();
        });

        let mut entity_list = Vec::with_capacity(1);
        let name = "eth0".to_string();

        let queues: usize = config
            .queues
            .parse()
            .with_context(|| format!("Invalid queues value: {}", config.queues))
            .unwrap_or(1) * 2;

        //sjtodo, queue.num and queue.size settings
        let endpoint: Arc<dyn Endpoint> = Arc::new(
            TapEndpoint::new(
                &handle,
                &name,
                &config.device_name,
                &config.device_hwaddr,
                queues,
                256,
                dev_mgr,
            )
            .await
            .context("Failed to create TapEndpoint")?,
        );

        let network_info = Arc::new(
            NetworkInfoFromXlet::new(config)
                .await
                .context("Failed to create NetworkInfoFromXlet")?,
        );

        entity_list.push(NetworkEntity {
            endpoint,
            network_info,
        });

        Ok(Self { entity_list })
    }
}

#[async_trait]
impl Network for Xlet {
    async fn setup(&self) -> Result<()> {
        let inner = self.inner.read().await;
        for e in inner.entity_list.iter() {
            e.endpoint.attach().await.context("Attach")?;
        }
        Ok(())
    }

    async fn interfaces(&self) -> Result<Vec<agent::Interface>> {
        let inner = self.inner.read().await;
        let mut interfaces = Vec::new();
        for e in inner.entity_list.iter() {
            interfaces.push(
                e.network_info
                    .interface()
                    .await
                    .context(format!("Failed to get interface for entity: {:?}", e))?,
            );
        }
        Ok(interfaces)
    }

    async fn routes(&self) -> Result<Vec<agent::Route>> {
        let inner = self.inner.read().await;
        let mut routes = Vec::new();
        for e in inner.entity_list.iter() {
            let mut list = e
                .network_info
                .routes()
                .await
                .context(format!("Failed to get routes for entity: {:?}", e))?;
            routes.append(&mut list);
        }
        Ok(routes)
    }

    async fn neighs(&self) -> Result<Vec<agent::ARPNeighbor>> {
        let inner = self.inner.read().await;
        let mut neighs = Vec::new();
        for e in &inner.entity_list {
            let mut list = e
                .network_info
                .neighs()
                .await
                .context(format!("Failed to get neighbors for entity: {:?}", e))?;
            neighs.append(&mut list);
        }
        Ok(neighs)
    }

    async fn save(&self) -> Option<Vec<EndpointState>> {
        let inner = self.inner.read().await;
        let mut ep_states = Vec::new();
        for e in &inner.entity_list {
            if let Some(state) = e.endpoint.save().await {
                ep_states.push(state);
            }
        }
        if ep_states.is_empty() {
            None
        } else {
            Some(ep_states)
        }
    }

    async fn remove(&self, h: &dyn Hypervisor) -> Result<()> {
        let inner = self.inner.read().await;

        for e in inner.entity_list.iter() {
            e.endpoint
                .detach(h)
                .await
                .context(format!("Failed to detach endpoint: {:?}", e.endpoint))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XletNetworkConfig {
    pub device_name: String,
    pub device_hwaddr: String,
    pub device_ip: String,
    pub mtu: String,
    pub routes: String,
    pub neighs: String,
    pub queues: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XletConfig {
    devices: Vec<XletDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XletDevice {
    // Name of device (interface name on the guest)
    pub name: String,
    // Mac address of interface on the guest, if it is not specified, a
    // private address is generated as default.
    #[serde(default = "generate_private_mac_addr")]
    pub guest_mac: String,
    // Device
    pub device: Device,
    // Network info
    pub network_info: NetworkInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Device {
    #[serde(rename = "host-tap")]
    HostTap {
        tap_name: String,
        #[serde(default = "default_queue_num")]
        queue_num: usize,
        #[serde(default = "default_queue_size")]
        queue_size: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub interface: Interface,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub neighbors: Vec<ARPNeighbor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interface {
    // IP addresses in the format of CIDR
    pub ip_addresses: Vec<String>,
    #[serde(default = "default_mtu")]
    pub mtu: u64,
    #[serde(default)]
    pub ntype: String,
    #[serde(default)]
    pub flags: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(default)]
    // Destination(CIDR), an empty string denotes no destination
    pub dest: String,
    #[serde(default)]
    // Gateway(IP Address), an empty string denotes no gateway
    pub gateway: String,
    #[serde(default)]
    // Source(IP Address), an empty string denotes no source
    pub source: String,
    #[serde(default)]
    pub scope: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ARPNeighbor {
    // IP address in the format of CIDR
    pub ip_address: Option<String>,
    #[serde(default)]
    pub hardware_addr: String,
    #[serde(default)]
    pub state: u32,
    #[serde(default)]
    pub flags: u32,
}

fn default_mtu() -> u64 {
    1400
}

fn default_queue_num() -> usize {
    2
}

fn default_queue_size() -> usize {
    256
}
