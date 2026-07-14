// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
// Copyright (c) 2022 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use safe_path::scoped_join;
use std::convert::TryFrom;
use std::path::PathBuf;

use crate::ch::convert::ProtectionDevConfig;
use super::inner::CloudHypervisorInner;
use crate::{
    BlockConfig, BlockDevice, device::pci_path::PciPath, device::DeviceType,
    HybridVsockDevice, NetworkConfig, NetworkDevice, NetworkConfigInfo,
    ProtectionDeviceConfig, ShareFsConfig, ShareFsDevice, ShareFsMountConfig,
    ShareFsMountOperation, ShareFsMountType, VfioDevice, VhostUserNetDevice,
    VmmState,
};
use anyhow::{anyhow, Context, Result};
use super::convert::DEFAULT_NUM_PCI_SEGMENTS;
use crate::ch::utils::PciDeviceInfo;
use kata_types::config::hypervisor::DEFAULT_RATE_LIMITER_REFILL_TIME;
use vmm::api::VmRemoveDeviceData;
use vmm::config::ImageType;
use vmm::net_util::mac::MacAddr;
use vmm::vm_config::{
    DeviceConfig, DiskConfig, FsConfig, FsMountConfigInfo, LockGranularityChoice, 
    NetConfig, PciDeviceCommonConfig, VhostMode, VsockConfig,
};
use vmm::virtio_devices::fs::BackendFsConfig;
use vmm::virtio_devices::{RateLimiterConfig, TokenBucketConfig};

const VIRTIO_FS: &str = "virtio-fs";
const INLINE_VIRTIO_FS: &str = "inline-virtio-fs";

pub const DEFAULT_FS_QUEUES: usize = 1;
const DEFAULT_FS_QUEUE_SIZE: u16 = 1024;

impl CloudHypervisorInner {
    pub(crate) async fn add_device(&mut self, device: DeviceType) -> Result<DeviceType> {
        if self.state != VmmState::VmRunning {
            info!(sl!(), "VMM not ready, queueing device {}", device);
            // If the VM is not running, add the device to the pending list to
            // be handled later.
            //
            // Note that the only device types considered are DeviceType::ShareFs
            // and DeviceType::Network since:
            //
            // - ShareFs (virtiofsd) is only needed in an non-DM and non-TDX scenario
            //   for the container rootfs.
            //
            // - For all other scenarios, the container rootfs is handled by a
            //   DeviceType::Block and this method is called *after* the VM
            //   has started so the device does not need to be added to the
            //   pending list.
            //
            // - The VM rootfs is handled without waiting for calls to this
            //   method as the file in question (image= or initrd=) is available
            //   from HypervisorConfig.BootInfo.{image,initrd}
            //   (see 'convert.rs').
            //
            // - Network details need to be saved for later application.
            //
            match device {
                DeviceType::ShareFs(_) => self.pending_devices.insert(0, device.clone()),
                DeviceType::Network(_) => self.pending_devices.insert(0, device.clone()),
                DeviceType::Vfio(_) => self.pending_devices.insert(0, device.clone()),
                DeviceType::VhostUserNetwork(_) => self.pending_devices.insert(0, device.clone()),
                DeviceType::Protection(_) => self.pending_devices.insert(0, device.clone()),
                _ => {
                    debug!(
                        sl!(),
                        "ignoring early add device request for device: {:?}", device
                    );
                }
            }

            return Ok(device);
        }

        info!(sl!(), "cloudhypervisor add device {:?}", &device);

        self.handle_add_device(device).await
    }

    async fn handle_add_device(&mut self, device: DeviceType) -> Result<DeviceType> {
        match device {
            DeviceType::ShareFs(sharefs) => self.handle_share_fs_device(sharefs).await,
            DeviceType::HybridVsock(hvsock) => self.handle_hvsock_device(hvsock).await,
            DeviceType::Block(block) => self.handle_block_device(block).await,
            DeviceType::Vfio(vfiodev) => self.handle_vfio_device(vfiodev).await,
            DeviceType::Network(netdev) => self.handle_network_device(netdev).await,
            DeviceType::VhostUserNetwork(vhostuser_netdev) => self.handle_vhostuser_network_device(vhostuser_netdev).await,
            _ => Err(anyhow!("unhandled device: {:?}", device)),
        }
    }

    /// Add the device that were requested to be added before the VMM was
    /// started.
    #[allow(dead_code)]
    pub(crate) async fn handle_pending_devices_after_boot(&mut self) -> Result<()> {
        if self.state != VmmState::VmRunning {
            return Err(anyhow!(
                "cannot handle pending devices with VMM state {:?}",
                self.state
            ));
        }

        while let Some(dev) = self.pending_devices.pop() {
            self.add_device(dev).await.context("add_device")?;
        }

        Ok(())
    }

    pub(crate) async fn remove_device(&mut self, device: DeviceType) -> Result<()> {
        match device {
            DeviceType::Vfio(vfiodev) => self.remove_vfio_device(&vfiodev).await,
            DeviceType::Block(block) => self.remove_block_device(&block).await,
            _ => Ok(()),
        }
    }

    pub(crate) async fn update_device(&mut self, device: DeviceType) -> Result<()> {
        info!(sl!(), "cloudhypervisor update device {:?}", &device);
        match device {
            DeviceType::ShareFs(sharefs_mount) => {
                // It's safe to unwrap mount config as mount_config is always there.
                self.add_share_fs_mount(&sharefs_mount.config.mount_config.unwrap())
                    .await.context("update share-fs device with mount operation.")
            }
            _ => Err(anyhow!("unsupported device {:?} to update.", device)),
        }
    }

    fn parse_inline_virtiofs_args(&mut self, options: &mut Vec<String>) -> Result<Option<BackendFsConfig>> {
        let mut debug = false;
        let mut opt_list = String::new();
        let mut bfs_cfg = BackendFsConfig::default();

        bfs_cfg.killpriv_v2 = true;

        let config = &self.config;
        info!(
            sl!(),
            "args: {:?}", config.shared_fs.virtio_fs_extra_args
        );
        let mut args = config.shared_fs.virtio_fs_extra_args.clone();
        let _ = go_flag::parse_args_with_warnings::<String, _, _>(&args, None, |flags| {
            flags.add_flag("d", &mut debug);
            flags.add_flag("thread-pool-size", &mut bfs_cfg.thread_pool_size);
            flags.add_flag("drop-sys-resource", &mut bfs_cfg.drop_sys_resource);
            flags.add_flag("o", &mut opt_list);
        })
        .with_context(|| format!("parse args: {:?}", args))?;

        // more options parsed for inline virtio-fs' custom config
        args.append(options);

        if debug {
            warn!(
                sl!(),
                "Inline virtiofs \"-d\" option not implemented, ignore"
            );
        }

        // Parse comma separated option list
        if !opt_list.is_empty() {
            let args: Vec<&str> = opt_list.split(',').collect();
            for arg in args {
                match arg {
                    "cache=none" => bfs_cfg.cache = 2,
                    "cache=auto" => bfs_cfg.cache = 0,
                    "cache=always" => bfs_cfg.cache = 1,
                    "no_open" => bfs_cfg.no_open = true,
                    "open" => bfs_cfg.no_open = false,
                    "writeback_cache" => bfs_cfg.writeback_cache = true,
                    "no_writeback_cache" => bfs_cfg.writeback_cache = false,
                    "writeback" => bfs_cfg.writeback_cache = true,
                    "no_writeback" => bfs_cfg.writeback_cache = false,
                    "xattr" => bfs_cfg.xattr = true,
                    "no_xattr" => bfs_cfg.xattr = false,
                    "cache_symlinks" => {} // inline virtiofs always cache symlinks
                    "no_readdir" => bfs_cfg.no_readdir = true,
                    "trace" => warn!(
                        sl!(),
                        "Inline virtiofs \"-o trace\" option not supported yet, ignored."
                    ),
                    _ => warn!(sl!(), "Inline virtiofs unsupported option: {}", arg),
                }
            }
        }

        debug!(sl!(), "Inline virtiofs config {:?}", bfs_cfg);
        Ok(Some(bfs_cfg))
    }

    async fn handle_share_fs_device(&mut self, sharefs: ShareFsDevice) -> Result<DeviceType> {
        let device = sharefs.clone();
        let mut bfs_config = None;
        let mut socket_path = PathBuf::new();

        match &device.config.fs_type as &str {
            VIRTIO_FS => {
                socket_path = if device.config.sock_path.starts_with('/') {
                    PathBuf::from(device.config.sock_path)
                } else {
                    scoped_join(&self.vm_path, device.config.sock_path)?
                };
            }
            INLINE_VIRTIO_FS => {
                let mut options: Vec<String> = device.config.options.clone();
                bfs_config = self.parse_inline_virtiofs_args(&mut options)?;
            }
            _ => {
                return Err(anyhow!(
                    "hypervisor isn't configured with shared_fs supported"
                ));
            }
        }

        let num_queues: usize = if device.config.queue_num > 0 {
            device.config.queue_num as usize
        } else {
            DEFAULT_FS_QUEUES
        };

        let queue_size: u16 = if device.config.queue_num > 0 {
            u16::try_from(device.config.queue_size)?
        } else {
            DEFAULT_FS_QUEUE_SIZE
        };

        let fs_config = FsConfig {
            tag: device.config.mount_tag,
            socket: socket_path,
            num_queues,
            queue_size,
            pci_common: PciDeviceCommonConfig {
                pci_segment: DEFAULT_NUM_PCI_SEGMENTS,
                ..Default::default()
            },
            backendfs_config: bfs_config,

            ..Default::default()
        };

        let response = self.vmm_instance.vm_add_fs(
            fs_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "fs add response: {:?}", detail);
        }

        Ok(DeviceType::ShareFs(sharefs))
    }

    async fn handle_vfio_device(&mut self, device: VfioDevice) -> Result<DeviceType> {
        let mut vfio_device: VfioDevice = device.clone();

        // A device with multi-funtions, or a IOMMU group with one more
        // devices, the Primary device is selected to be passed to VM.
        // And the the first one is Primary device.
        // safe here, devices is not empty.
        let primary_device = device.devices.first().ok_or(anyhow!(
            "Primary device list empty for vfio device {:?}",
            device
        ))?;

        let primary_device = primary_device.clone();

        let sysfsdev = primary_device.sysfs_path.clone();

        let device_config = DeviceConfig {
            path: PathBuf::from(sysfsdev),
            pci_common: PciDeviceCommonConfig {
                iommu: false,
                ..Default::default()
            },
            x_nv_gpudirect_clique: self.alloc_x_nv_gpudirect_clique(),
            x_exclude_mmap_bars: Vec::new(),
        };

        let response = self.vmm_instance.vm_add_device(
            device_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "VFIO add response: {:?}", detail);

            // Store the cloud-hypervisor device id to be used later for remving the device
            let dev_info: PciDeviceInfo =
                serde_json::from_str(detail.as_str()).map_err(|e| anyhow!(e))?;
            self.device_ids
                .insert(device.device_id.clone(), dev_info.id);

            // Update PCI path for the vfio host device. It is safe to directly access the slice element
            // here as we have already checked if it exists.
            // Todo: Handle vfio-ap mediated devices - return error for them.
            vfio_device.devices[0].guest_pci_path =
                Some(Self::clh_pci_info_to_path(&dev_info.bdf)?);
        }

        Ok(DeviceType::Vfio(vfio_device))
    }

    async fn remove_vfio_device(&mut self, device: &VfioDevice) -> Result<()> {
        let clh_device_id = self.device_ids.get(&device.device_id);

        if clh_device_id.is_none() {
            return Err(anyhow!(
                "Device id for cloud-hypervisor not found while removing device"
            ));
        }

        let clh_device_id = clh_device_id.unwrap();
        let rm_data = VmRemoveDeviceData {
            id: clh_device_id.clone(),
        };

        let response = self.vmm_instance.vm_remove_device(
            rm_data,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "vfio remove response: {:?}", detail);
        }

        Ok(())
    }

    async fn remove_block_device(&mut self, device: &BlockDevice) -> Result<()> {
        let clh_device_id = self.device_ids.get(&device.device_id);

        if clh_device_id.is_none() {
            return Err(anyhow!(
                "Device id for cloud-hypervisor not found while removing block device"
            ));
        }

        let clh_device_id = clh_device_id.unwrap();
        let rm_data = VmRemoveDeviceData {
            id: clh_device_id.clone(),
        };

        let response = self.vmm_instance.vm_remove_device(
            rm_data,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "block remove response: {:?}", detail);
        }

        Ok(())
    }

    // Various cloud-hypervisor APIs report a PCI address in "BB:DD.F"
    // form within the PciDeviceInfo struct.
    // eg "0000:00:DD.F"
    fn clh_pci_info_to_path(bdf: &str) -> Result<PciPath> {
        let tokens: Vec<&str> = bdf.split(':').collect();
        if tokens.len() != 3 || tokens[0] != "0000" || tokens[1] != "00" {
            return Err(anyhow!(
                "Unexpected PCI address {:?} for clh device add",
                bdf
            ));
        }

        let toks: Vec<&str> = tokens[2].split('.').collect();
        if toks.len() != 2 || toks[1] != "0" || toks[0].len() != 2 {
            return Err(anyhow!(
                "Unexpected PCI address {:?} for clh device add",
                bdf
            ));
        }

        PciPath::try_from(toks[0])
    }

    async fn handle_hvsock_device(&mut self, device: HybridVsockDevice) -> Result<DeviceType> {
        let hvsock_config = device.config.clone();

        let vsock_config = VsockConfig {
            pci_common: PciDeviceCommonConfig {
                ..Default::default()
            },
            cid: hvsock_config.guest_cid.into(),
            socket: hvsock_config.uds_path.into(),
        };

        let response = self.vmm_instance.vm_add_vsock(
            vsock_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "hvsock add response: {:?}", detail);
        }

        Ok(DeviceType::HybridVsock(device))
    }

    async fn handle_block_device(&mut self, device: BlockDevice) -> Result<DeviceType> {
        let mut block_dev = device.clone();

        let mut disk_config = DiskConfig::try_from(device.config.clone())?;
        disk_config.direct = device
            .config
            .is_direct
            .unwrap_or(self.config.blockdev_info.block_device_cache_direct);

        if self.config.blockdev_info.disk_rate_limiter_bw_max_rate > 0
        || self.config.blockdev_info.disk_rate_limiter_ops_max_rate > 0 {
            let block_rate_limit = RateLimiterConfig {
                bandwidth: if self.config.blockdev_info.disk_rate_limiter_bw_max_rate > 0 {
                    Some(TokenBucketConfig {
                        size: self.config.blockdev_info.disk_rate_limiter_bw_max_rate,
                        one_time_burst: self.config.blockdev_info.disk_rate_limiter_bw_one_time_burst,
                        refill_time: DEFAULT_RATE_LIMITER_REFILL_TIME,
                    })
                } else {
                    None
                },
                ops: if self.config.blockdev_info.disk_rate_limiter_ops_max_rate > 0 {
                    Some(TokenBucketConfig {
                        size: self.config.blockdev_info.disk_rate_limiter_ops_max_rate,
                        one_time_burst: self.config.blockdev_info.disk_rate_limiter_ops_one_time_burst,
                        refill_time: DEFAULT_RATE_LIMITER_REFILL_TIME,
                    })
                } else {
                    None
                },

            };

            disk_config.rate_limiter_config = Some(block_rate_limit);
        }

        let response = self.vmm_instance.vm_add_disk(
            disk_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "blockdev add response: {:?}", detail);

            let dev_info: PciDeviceInfo =
                serde_json::from_str(detail.as_str()).map_err(|e| anyhow!(e))?;
            self.device_ids.insert(device.device_id, dev_info.id);
            block_dev.config.pci_path = Some(Self::clh_pci_info_to_path(dev_info.bdf.as_str())?);
        }

        if block_dev.config.is_overlayfs {
            self.overlayfs_block_device = Some(DeviceType::Block(block_dev.clone()));
        }

        Ok(DeviceType::Block(block_dev))
    }

    async fn handle_network_device(&mut self, device: NetworkDevice) -> Result<DeviceType> {
        let netdev = device.clone();
        let mut netdev_config = netdev.config.clone();
        netdev_config.queue_num = std::cmp::min(
            (self.config.cpu_info.current_vcpus * 2.0) as usize, netdev_config.queue_num);

        let clh_net_config = NetConfig::try_from(NetConfigInner::new(self.config.network_info.clone(), netdev_config))?;

        let response = self.vmm_instance.vm_add_net(
            clh_net_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "netdev add response: {:?}", detail);
        }

        Ok(DeviceType::Network(netdev))
    }

    /// Add vhost-user-net deivce to cloud-hypervisor
    async fn handle_vhostuser_network_device(&mut self, device: VhostUserNetDevice) -> Result<DeviceType> {
        let vhostuser_netdev = device.clone();
        let mut vhostuser_device = device.clone();
        vhostuser_device.config.num_queues = std::cmp::min(
            (self.config.cpu_info.current_vcpus * 2.0) as usize, vhostuser_device.config.num_queues);
        
        let vhost_net_config = NetConfig::try_from(VhostUserNetDeviceInner::new(self.config.network_info.clone(), vhostuser_device))?;

        let response = self.vmm_instance.vm_add_net(
            vhost_net_config,
        )
        .await?;

        if let Some(detail) = response {
            debug!(sl!(), "vhost-user net add response: {:?}", detail);
        }

        Ok(DeviceType::VhostUserNetwork(vhostuser_netdev))
    }

    pub(crate) async fn get_shared_devices(
        &mut self,
    ) -> Result<(
        Option<Vec<FsConfig>>,
        Option<Vec<NetConfig>>,
        Option<Vec<DeviceConfig>>,
        Option<ProtectionDevConfig>,
    )> {
        let mut shared_fs_devices = Vec::<FsConfig>::new();
        let mut network_devices = Vec::<NetConfig>::new();
        let mut host_devices = Vec::<DeviceConfig>::new();
        let mut protection_device = ProtectionDevConfig::default();

        while let Some(dev) = self.pending_devices.pop() {
            match dev {
                DeviceType::ShareFs(dev) => {
                    let device: ShareFsDevice = dev.clone();
                    let mut bfs_config = None;

                    match &device.config.fs_type as &str {
                        VIRTIO_FS => {()}
                        INLINE_VIRTIO_FS => {
                            let mut options: Vec<String> = device.config.options.clone();
                            bfs_config = self.parse_inline_virtiofs_args(&mut options)?;
                        }
                        _ => {
                            return Err(anyhow!(
                                "hypervisor isn't configured with shared_fs supported"
                            ));
                        }
                    }

                    let settings = ShareFsSettings::new(
                        dev.config,
                        self.vm_path.clone(),
                        bfs_config,
                    );

                    let fs_cfg = FsConfig::try_from(settings)?;

                    shared_fs_devices.push(fs_cfg);
                }
                DeviceType::Network(net_device) => {
                    let mut net_config = NetConfig::try_from(NetConfigInner::new(self.config.network_info.clone(), net_device.config))?;
                    net_config.num_queues = std::cmp::min(
                        (self.config.cpu_info.current_vcpus * 2.0) as usize, net_config.num_queues);
                    network_devices.push(net_config);
                }
                DeviceType::VhostUserNetwork(vhostuser_netdev) => {
                    let mut vhost_net_config = NetConfig::try_from(VhostUserNetDeviceInner::new(self.config.network_info.clone(), vhostuser_netdev))?;
                    vhost_net_config.num_queues = std::cmp::min(
                        (self.config.cpu_info.current_vcpus * 2.0) as usize, vhost_net_config.num_queues);
                    network_devices.push(vhost_net_config);
                }
                DeviceType::Vfio(vfio_device) => {
                    // A device with multi-funtions, or a IOMMU group with one more
                    // devices, the Primary device is selected to be passed to VM.
                    // And the the first one is Primary device.
                    // safe here, devices is not empty.
                    let primary_device = vfio_device.devices.first().ok_or(anyhow!(
                        "Primary device list empty for vfio device {:?}",
                        vfio_device
                    ))?;

                    let primary_device = primary_device.clone();
                    let sysfsdev = primary_device.sysfs_path.clone();
                    let device_config = DeviceConfig {
                        path: PathBuf::from(sysfsdev),
                        pci_common: PciDeviceCommonConfig {
                            iommu: false,
                            ..Default::default()
                        },
                        x_nv_gpudirect_clique: self.alloc_x_nv_gpudirect_clique(),
                        x_exclude_mmap_bars: Vec::new(),
                    };
                    info!(
                        sl!(),
                        "get host_devices primary device {:?}", primary_device
                    );
                    host_devices.push(device_config);
                }
                DeviceType::Protection(pdev) => {
                    let config = pdev.config;
                    match config {
                        ProtectionDeviceConfig::SevSnp(sevsnp_cfg) => {
                            if sevsnp_cfg.is_snp {
                                protection_device.host_data = sevsnp_cfg.host_data;
                            }
                        }
                        ProtectionDeviceConfig::Tdx(tdx_config) => {
                            protection_device.mrconfigid = tdx_config.mrconfigid;
                        }
                        _ => info!(sl!(), "CH: unsupported protection device type"),
                    }
                }
                _ => continue,
            }
        }

        Ok((
            Some(shared_fs_devices),
            Some(network_devices),
            Some(host_devices),
            Some(protection_device),
        ))
    }

    async fn add_share_fs_mount(&mut self, config: &ShareFsMountConfig) -> Result<()> {
        let ops = match config.op {
            ShareFsMountOperation::Mount => "mount",
            ShareFsMountOperation::Umount => "umount",
            ShareFsMountOperation::Update => "update",
        };

        let fstype = match config.fstype {
            ShareFsMountType::PASSTHROUGH => "passthroughfs",
            ShareFsMountType::RAFS => "rafs",
        };

        let cfg = FsMountConfigInfo {
            ops: ops.to_string(),
            fstype: Some(fstype.to_string()),
            old_source: None,
            source: Some(config.source.clone()),
            mountpoint: config.mount_point.clone(),
            config: config.config.clone(),
            tag: config.tag.clone(),
            prefetch_list_path: config.prefetch_list_path.clone(),
            dax_threshold_size_kb: None,
        };

        self.vmm_instance.vm_patch_fs(&cfg).await.map_err(|e| {
            anyhow!(
                "{:?} {} at {} error: {:?}",
                config.op,
                fstype,
                config.mount_point.clone(),
                e
            )
        })
    }
}

pub struct NetConfigInner {
    pub network_info: NetworkConfigInfo,
    pub cfg: NetworkConfig,
}

impl NetConfigInner {
    pub fn new(network_info: NetworkConfigInfo, cfg: NetworkConfig) -> Self {
        Self {
            network_info,
            cfg,
        }
    }
}

impl TryFrom<NetConfigInner> for NetConfig {
    type Error = anyhow::Error;

    fn try_from(net_config_inner: NetConfigInner) -> Result<Self, Self::Error> {
        if let Some(mac) = net_config_inner.cfg.guest_mac {
            let net_config: NetConfig = NetConfig {
                pci_common: PciDeviceCommonConfig {
                    id: Some(net_config_inner.cfg.virt_iface_name.clone()),
                    ..Default::default()
                },
                tap: Some(net_config_inner.cfg.host_dev_name.clone()),
                num_queues: net_config_inner.cfg.queue_num,
                queue_size: net_config_inner.cfg.queue_size as u16,
                mac: MacAddr { bytes: mac.0 },
                offload_csum: !net_config_inner.network_info.disable_offload_csum,
                offload_tso: !net_config_inner.network_info.disable_offload_tso,
                offload_ufo: !net_config_inner.network_info.disable_offload_ufo,
                ip: None,
                mask: None,
                host_mac: None,
                mtu: None,
                vhost_user: false,
                vhost_socket: None,
                vhost_mode: VhostMode::Client,
                fds: None,
                rate_limiter_config: None,
            };

            return Ok(net_config);
        }

        Err(anyhow!("Missing mac address for network device"))
    }
}

pub struct VhostUserNetDeviceInner {
    pub network_info: NetworkConfigInfo,
    pub device: VhostUserNetDevice,
}

impl VhostUserNetDeviceInner {
    pub fn new(network_info: NetworkConfigInfo, device: VhostUserNetDevice) -> Self {
        Self {
            network_info,
            device,
        }
    }
}

impl TryFrom<VhostUserNetDeviceInner> for NetConfig {
    type Error = anyhow::Error;

    fn try_from(vhost_user_net_device_inner: VhostUserNetDeviceInner) -> Result<Self, Self::Error> {
        let guest_mac = MacAddr::parse_str(&vhost_user_net_device_inner.device.config.mac_address).ok().unwrap();
        let net_config = NetConfig {
            pci_common: PciDeviceCommonConfig {
                id: Some(vhost_user_net_device_inner.device.device_id.clone()),
                ..Default::default()
            },
            num_queues: vhost_user_net_device_inner.device.config.num_queues,
            queue_size: vhost_user_net_device_inner.device.config.queue_size as u16,
            vhost_user: true,
            vhost_socket: Some(vhost_user_net_device_inner.device.config.socket_path.clone()),  
            mac: guest_mac,
            vhost_mode: VhostMode::Server,
            offload_csum: !vhost_user_net_device_inner.network_info.disable_offload_csum,
            offload_tso: !vhost_user_net_device_inner.network_info.disable_offload_tso,
            offload_ufo: !vhost_user_net_device_inner.network_info.disable_offload_ufo,
            tap: None,
            ip: None,
            mask: None,
            host_mac: None,
            mtu: None,
            fds: None,
            rate_limiter_config: None,
        };

        return Ok(net_config);
    }
}

impl TryFrom<BlockConfig> for DiskConfig {
    type Error = anyhow::Error;

    fn try_from(blkcfg: BlockConfig) -> Result<Self, Self::Error> {
        let disk_config: DiskConfig = DiskConfig {
            path: Some(blkcfg.path_on_host.as_str().into()),
            readonly: blkcfg.is_readonly,
            num_queues: blkcfg.num_queues,
            queue_size: blkcfg.queue_size as u16,
            image_type: ImageType::Raw,
            pci_common: PciDeviceCommonConfig {
                ..Default::default()
            },
            direct: blkcfg.is_direct.unwrap_or_default(),
            vhost_user: false,
            vhost_socket: None,
            rate_limit_group: None,
            rate_limiter_config: None,
            disable_io_uring: false,
            disable_aio: false,
            serial: None,
            queue_affinity: None,
            backing_files: false,
            sparse: false,
            lock_granularity: LockGranularityChoice::default(),
        };

        Ok(disk_config)
    }
}

#[derive(Debug)]
pub struct ShareFsSettings {
    cfg: ShareFsConfig,
    vm_path: String,
    backendfs_config: Option<BackendFsConfig>,
}

impl ShareFsSettings {
    pub fn new(cfg: ShareFsConfig, vm_path: String, bfs_config: Option<BackendFsConfig>) -> Self {
        ShareFsSettings {
            cfg,
            vm_path,
            backendfs_config: bfs_config,
        }
    }
}

impl TryFrom<ShareFsSettings> for FsConfig {
    type Error = anyhow::Error;

    fn try_from(settings: ShareFsSettings) -> Result<Self, Self::Error> {
        let cfg = settings.cfg;
        let vm_path = settings.vm_path;
        let bfs_config = settings.backendfs_config;
        let mut socket_path = PathBuf::new();

        let num_queues: usize = if cfg.queue_num > 0 {
            cfg.queue_num as usize
        } else {
            DEFAULT_FS_QUEUES
        };

        let queue_size: u16 = if cfg.queue_num > 0 {
            u16::try_from(cfg.queue_size)?
        } else {
            DEFAULT_FS_QUEUE_SIZE
        };

        if bfs_config.is_none() {
            socket_path = if cfg.sock_path.starts_with('/') {
                PathBuf::from(cfg.sock_path)
            } else {
                PathBuf::from(vm_path).join(cfg.sock_path)
            };
        }

        let fs_cfg = FsConfig {
            tag: cfg.mount_tag,
            socket: socket_path,
            num_queues,
            queue_size,
            backendfs_config: bfs_config,
            ..Default::default()
        };

        Ok(fs_cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Address;

    #[test]
    fn test_networkconfig_to_netconfig() {
        let mut cfg = NetworkConfig {
            host_dev_name: String::from("tap0"),
            virt_iface_name: String::from("eth0"),
            queue_size: 256,
            queue_num: 2,
            guest_mac: None,
            index: 1,
            allow_duplicate_mac: false,
            use_generic_irq: None,
            use_shared_irq: None,
        };

        let network_info = NetworkConfigInfo {
            disable_offload_csum: false,
            disable_offload_tso: false,
            disable_offload_ufo: false,
            ..Default::default()
        };

        let net = NetConfig::try_from(NetConfigInner::new(network_info.clone(), cfg.clone()));
        assert_eq!(
            net.unwrap_err().to_string(),
            "Missing mac address for network device"
        );

        let v: [u8; 6] = [10, 11, 128, 3, 4, 5];
        let mac_address = Address(v);
        cfg.guest_mac = Some(mac_address.clone());

        let expected = NetConfig {
            pci_common: PciDeviceCommonConfig {
                id: Some(cfg.virt_iface_name.clone()),
                ..Default::default()
            },
            num_queues: cfg.queue_num,
            queue_size: cfg.queue_size as u16,
            mac: MacAddr { bytes: v },
            ..Default::default()
        };

        let net = NetConfig::try_from(NetConfigInner::new(network_info.clone(), cfg.clone()));
        assert!(net.is_ok());
        assert_eq!(net.unwrap(), expected);
    }
}
