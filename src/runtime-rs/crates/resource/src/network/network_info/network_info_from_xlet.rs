// Copyright (c) 2019-2023 Alibaba Cloud
// Copyright (c) 2019-2023 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use agent::{ARPNeighbor, IPAddress, Interface, Route};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use super::NetworkInfo;
use crate::network::utils::address::{ip_family_from_ip_addr, parse_ip_cidr};
use crate::network::xlet::XletNetworkConfig;

#[derive(Debug)]
pub(crate) struct NetworkInfoFromXlet {
    interface: Interface,
    routes: Vec<Route>,
    neighs: Vec<ARPNeighbor>,
}

impl NetworkInfoFromXlet {
    pub async fn new(xlet_device: &XletNetworkConfig) -> Result<Self> {
        // Parse device IP address
        let (ipaddr, mask) = match parse_ip_cidr(&xlet_device.device_ip) {
            Ok(ip_cidr) => (ip_cidr.0, ip_cidr.1),
            Err(e) => return Err(anyhow::anyhow!("Failed to parse IP CIDR: {}", e)),
        };
        let device_ip_address = IPAddress {
            family: ip_family_from_ip_addr(&ipaddr),
            address: ipaddr.to_string(),
            mask: format!("{}", mask),
        };

        // Parse MTU, handle potential parsing errors
        let mtu: u64 = xlet_device
            .mtu
            .parse()
            .with_context(|| format!("Invalid MTU value: {}", xlet_device.mtu))
            .unwrap_or(1500);

        let interface = Interface {
            device: xlet_device.device_name.clone(),
            name: xlet_device.device_name.clone(),
            ip_addresses: vec![device_ip_address],
            mtu,
            hw_addr: xlet_device.device_hwaddr.clone(),
            ..Default::default()
        };

        // Parse routes
        let route_parts: Vec<&str> = xlet_device.routes.trim().split(',').collect();
        if route_parts.len() < 2 {
            return Err(anyhow!("Invalid routes format: {:?}", xlet_device.routes));
        }
        let route_dest = route_parts[0].to_string();
        let route_gateway = route_parts[1].to_string();

        let route = Route {
            dest: route_dest,
            gateway: route_gateway,
            source: ipaddr.to_string(),
            device: xlet_device.device_name.clone(),
            scope: 0,
            family: ip_family_from_ip_addr(&ipaddr),
            flags: 4,
            mtu: mtu as u32,
        };
        let routes = vec![route];

        // Parse neighbors
        let neigh_parts: Vec<&str> = xlet_device.neighs.trim().split(',').collect();
        if neigh_parts.len() < 2 {
            return Err(anyhow!(
                "Invalid neighbors format: {:?}",
                xlet_device.neighs
            ));
        }
        let neigh_gateway = neigh_parts[0].to_string();
        let neigh_hwaddr = neigh_parts[1].to_string();

        let neigh_ip_address = IPAddress {
            family: agent::IPFamily::V4,
            address: neigh_gateway.to_string(),
            mask: format!("{}", 32),
        };

        let neigh = ARPNeighbor {
            to_ip_address: Some(neigh_ip_address),
            device: xlet_device.device_name.clone(),
            ll_addr: neigh_hwaddr,
            state: 0,
            flags: 0,
        };
        let neighs = vec![neigh];

        Ok(Self {
            interface,
            routes,
            neighs,
        })
    }
}

#[async_trait]
impl NetworkInfo for NetworkInfoFromXlet {
    async fn interface(&self) -> Result<Interface> {
        Ok(self.interface.clone())
    }

    async fn routes(&self) -> Result<Vec<Route>> {
        Ok(self.routes.clone())
    }

    async fn neighs(&self) -> Result<Vec<ARPNeighbor>> {
        Ok(self.neighs.clone())
    }
}
