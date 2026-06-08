// Copyright (c) 2019-2022 Ctyunos
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use std::{
    fs::File, os::unix::prelude::AsRawFd, rc::Rc, sync::{mpsc::{channel, Sender}, Arc, Mutex, RwLock}, thread
};

use anyhow::{Context, Result};
use vmm::{
    api::{http::{http_api_graceful_shutdown, HttpApiHandle}, ApiAction, ApiRequest, VmRemoveDeviceData, VmResizeData, VmSnapshotConfig, VmmPingResponse},
    config::{DeviceConfig, DiskConfig, FsConfig, FsMountConfigInfo, NetConfig, RestoreConfig, VmConfig, VsockConfig},
    seccomp_filters::{get_seccomp_filter, Thread},
    VmmVersionInfo,
};  
use nix::sched::{setns, CloneFlags};
use vmm_sys_util_v2::eventfd::EventFd;
use signal_hook::consts::SIGSYS;
use seccompiler_v2::{apply_filter, SeccompAction};
use serde::{Serialize, Deserialize};
use micro_http::Body;

const BUILD_VERSION: &str = "40.0.0";

#[derive(Debug, Deserialize, Serialize)]
pub struct InstanceInfo {
    pub master_tid: u32,
}

impl InstanceInfo {
    pub fn new() -> Self {
        InstanceInfo {
            master_tid: 0,
        }
    }
}

impl Default for InstanceInfo {
    fn default() -> Self {
        InstanceInfo {
            master_tid: 0,
        }
    }
}

#[derive(Debug)]
pub struct VmmInstance {
    /// VMM instance info directly accessible from runtime
    vmm_shared_info: Arc<RwLock<InstanceInfo>>,
    api_event: EventFd,
    exit_event: EventFd,
    api_sender: Option<futures::lock::Mutex<Sender<ApiRequest>>>,
    vmm_thread: Option<thread::JoinHandle<Result<i32>>>,
    http_api_handle: Option<HttpApiHandle>,
}

impl VmmInstance {
    pub fn new(id: &str) -> Self {
        let vmm_shared_info = Arc::new(RwLock::new(InstanceInfo::new()));
        let api_event = EventFd::new(libc::EFD_NONBLOCK)
            .unwrap_or_else(|_| panic!("Failed to create eventfd for vmm {}", id));

        let exit_event = EventFd::new(libc::EFD_NONBLOCK).expect("Failed to create exit_evt");

        VmmInstance {
            vmm_shared_info,
            api_event,
            exit_event,
            api_sender: None,
            vmm_thread: None,
            http_api_handle: None,
        }
    }

    pub fn pid(&self) -> u32 {
        std::process::id()
    }

    pub fn get_vcpu_tids(&self) -> Vec<(u8, u32)> {
        let mut result: Vec<(u8, u32)>  = Vec::new();
        let path_name = format!("/proc/{}/task", self.pid());
        let path = std::path::Path::new(&path_name);

        if path.is_dir() {
            if let Ok(dir) = path.read_dir() {
                for entity in dir.flatten() {
                    let tid_path = entity.path();
                    let file_name = tid_path.file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or_default();

                    if let Ok(tid) = file_name.parse::<u32>() {
                        let comm_path = tid_path.join("comm");
                        if let Ok(comm_content) = std::fs::read_to_string(comm_path) {
                            let thread_name = comm_content.trim();
                            if thread_name.starts_with("vcpu") {
                                let cpu_num = thread_name[4..].parse::<u8>().unwrap();
                                result.push((cpu_num, tid));
                            }
                        }
                    }
                }
            }
        }
        result
    }

    pub fn get_vmm_master_tid(&self) -> u32 {
        let info = self.vmm_shared_info.clone();
        let result = info.read().unwrap().master_tid;
        result
    }

    pub fn get_shared_info(&self) -> Arc<RwLock<InstanceInfo>> {
        self.vmm_shared_info.clone()
    }

    pub fn get_ns_path(&self) -> String {
        let info_binding = self.vmm_shared_info.clone();
        let info = info_binding.read().unwrap();
        let result = format!("/proc/{}/task/{}/ns", self.pid(), info.master_tid);
        result
    }

    pub fn run_vmm_server(&mut self, id: &str, netns: Option<String>, secomp_value: Option<&str>, api_socket: &Option<String>) -> Result<()> {
        let hypervisor = vmm::hypervisor::new().unwrap_or_else(|_| panic!("Failed to create hypervisor for vmm {}", id));
        let api_event_fd = self.api_event.try_clone().expect("Failed to dup api_event.");

        let (api_request_sender, api_request_receiver) = channel();

        let seccomp_action = if let Some(seccomp_str) = secomp_value {
            match seccomp_str {
                "true" => SeccompAction::Trap,
                "false" => SeccompAction::Allow,
                "log" => SeccompAction::Log,
                val => {
                    // The user providing an invalid value will be rejected
                    panic!("Invalid parameter {} for \"--seccomp\" flag", val);
                }
            }
        } else {
            SeccompAction::Trap
        };

        if seccomp_action == SeccompAction::Trap {
            // SAFETY: We only using signal_hook for managing signals and only execute signal
            // handler safe functions (writing to stderr) and manipulating signals.
            unsafe {
                signal_hook::low_level::register(signal_hook::consts::SIGSYS, || {
                    eprint!(
                        "\n==== Possible seccomp violation ====\n\
                    Try running with `strace -ff` to identify the cause and open an issue: \
                    https://github.com/cloud-hypervisor/cloud-hypervisor/issues/new\n"
                    );
                    signal_hook::low_level::emulate_default_handler(SIGSYS).unwrap();
                })
            }
            .map_err(|e| eprintln!("Error adding SIGSYS signal handler: {e}"))
            .ok();
        }

        let vmm_shared_info = self.get_shared_info();

        let hypervisor_type = hypervisor.hypervisor_type();

        let vmm_seccomp_filter = get_seccomp_filter(&seccomp_action, Thread::Vmm, hypervisor_type)
                .expect("Failed to get seccomp filter.");

        let vmm_seccomp_action = seccomp_action.clone();
        let exit_event_clone = self.exit_event.try_clone().expect("Failed to dup exit_event.");
        let api_event_fd_clone = api_event_fd.try_clone().expect("Failed to dup api_event_fd.");

        let mut vmm_instance = vmm::Vmm::new(
            VmmVersionInfo::new(BUILD_VERSION, env!("CARGO_PKG_VERSION")),
            api_event_fd,
            vmm_seccomp_action,
            hypervisor,
            exit_event_clone,
        ).expect("Failed to create vmm instance.");
        self.vmm_thread = Some(
            thread::Builder::new()
                .name("vmm_master".to_string())
                .spawn(move || {
                    || -> Result<i32> {
                        debug!(sl!(), "run vmm thread start");
                        if !vmm_seccomp_filter.is_empty() {
                            apply_filter(&vmm_seccomp_filter).context("Failed to apply filter.")?;
                        }

                        let cur_tid = nix::unistd::gettid().as_raw() as u32;
                        vmm_shared_info.write().unwrap().master_tid = cur_tid;

                        if let Some(netns_path) = netns {
                            info!(sl!(), "set netns for vmm master {}", &netns_path);
                            let netns_fd = File::open(&netns_path)
                                .with_context(|| format!("open netns path {}", &netns_path))?;
                            setns(netns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET)
                                .context("set netns ")?;
                        }

                        vmm_instance.setup_signal_handler().expect("Failed to setup signal handler.");

                        vmm_instance.control_loop(
                            Rc::new(api_request_receiver),
                        ).expect("Failed to setup control loop.");
                        Ok(0)
                    }()
                    .map_err(|e| {
                        error!(sl!(), "run vmm thread err. {:?}", e);
                        e
                    })
                })
                .expect("Failed to start vmm event loop"),
        );
     
        self.http_api_handle = if let Some(http_path) = api_socket {
            info!(sl!(), "create vmm http sock");
            let api_request_sender_clone = api_request_sender.clone();
            let exit_event_new = self.exit_event.try_clone().expect("Failed to dup exit_event.");
            let handle = vmm::api::start_http_path_thread(
                http_path,
                api_event_fd_clone,
                api_request_sender_clone,
                &seccomp_action,
                exit_event_new,
                hypervisor_type,
            ).expect("Failed to create http thread.");
            Some(handle)
        } else {
            None
        };

        self.api_sender = Some(futures::lock::Mutex::new(api_request_sender.clone()));
 
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        // vmm is not running, join thread will be hang.
        if self.vmm_thread.is_none() {
            debug!(sl!(), "vmm-master thread is uninitialized or has exited.");
            return Ok(());
        }
        debug!(sl!(), "join vmm-master thread exit.");

        if let Err(e) = self.exit_event.write(1) {
            warn!(sl!(), "writing to exit EventFd: {e}");
        }

        // vmm_thread must be exited, otherwise there will be other sync issues.
        // unwrap is safe, if vmm_thread is None, impossible run to here.
        self.vmm_thread.take().unwrap().join().ok();
        info!(sl!(), "vmm-master thread join succeed.");

        self.vmm_thread = None;

        // Shutdown HTTP API if it exists
        if let Some(api_handle) = self.http_api_handle.take() {
            if let Err(e) = http_api_graceful_shutdown(api_handle) {
                warn!(sl!(), "failed to gracefully shutdown HTTP API: {:?}", e);
            }
        }
        
        Ok(())
    }

    async fn clone_api_sender(&mut self) -> Sender<ApiRequest> {
        // lock the async mutex, clone the `Sender` and then immediately
        // drop the MutexGuard so that other tasks can clone the
        // `Sender` as well
        self.api_sender.as_ref().unwrap().lock().await.clone()
    }

    fn clone_api_notifier(&mut self) -> Result<EventFd> {
        Ok(self.api_event
            .try_clone()
            .map_err(api_error)?)
    }

    async fn vm_action<Action: ApiAction<ResponseBody = Option<Body>>>(
        &mut self,
        action: &'static Action,
        body: Action::RequestBody,
    ) -> Result<Option<String>> {
        let api_sender = self.clone_api_sender().await;
        let api_notifier = self.clone_api_notifier()?;

        let result = blocking::unblock(move || action.send(api_notifier, api_sender, body))
            .await
            .map_err(api_error)?
            // We're using `from_utf8_lossy` here to not deal with the
            // error case of `from_utf8` as we know that `b.body` is valid JSON.
            .map(|b| String::from_utf8_lossy(&b.body).to_string());

        Ok(result.into())
    }

    pub async fn create_vm_instance(&mut self, vm_config: VmConfig) -> Result<()> {
        let api_sender = self.clone_api_sender().await;
        let api_notifier = self.clone_api_notifier()?;

        blocking::unblock(move || {
            vmm::api::VmCreate.send(api_notifier, api_sender, Arc::new(Mutex::new(vm_config)))
        })
        .await
        .map_err(api_error)?;

        Ok(())
    }

    pub async fn start_vm_instance(&mut self) -> Result<()> {
        self.vm_action(&vmm::api::VmBoot, ()).await.map(|_| ())
    }

    pub async fn vmm_ping(&mut self) -> Result<VmmPingResponse> {
        let api_sender = self.clone_api_sender().await;
        let api_notifier = self.clone_api_notifier()?;

        let result = blocking::unblock(move || vmm::api::VmmPing.send(api_notifier, api_sender, ()))
            .await
            .map_err(api_error)?;

        Ok(result)
    }

    pub async fn shutdown_vm_instance(&mut self) -> Result<()> {
        self.vm_action(&vmm::api::VmShutdown, ()).await.map(|_| ())
    }

    pub async fn vm_add_disk(&mut self, disk_config: DiskConfig) -> Result<Option<String>> {
        self.vm_action(&vmm::api::AddDisk, disk_config).await
    }

    pub async fn vm_add_net(&mut self, net_config: NetConfig) -> Result<Option<String>> {
        self.vm_action(&vmm::api::VmAddNet, net_config).await
    }

    pub async fn vm_add_device(&mut self, device_config: DeviceConfig) -> Result<Option<String>> {
        self.vm_action(&vmm::api::VmAddDevice, device_config).await
    }

    pub async fn vm_remove_device(&mut self, vm_remove_device: VmRemoveDeviceData) -> Result<Option<String>> {
        self.vm_action(&vmm::api::VmRemoveDevice, vm_remove_device).await
    }

    pub async fn vm_add_fs(&mut self, fs_config: FsConfig) -> Result<Option<String>> {
        self.vm_action(&vmm::api::VmAddFs, fs_config).await
    }

    pub async fn vm_add_vsock(&mut self, vsock_config: VsockConfig) -> Result<Option<String>> {
        self.vm_action(&vmm::api::VmAddVsock, vsock_config).await
    }

    pub async fn vm_patch_fs(&mut self, cfg: &FsMountConfigInfo) -> Result<()> {
        self.vm_action(&vmm::api::VmPatchFs, cfg.clone()).await.map(|_| ())
    }

    pub async fn vm_resize(&mut self, vm_resize_data: VmResizeData) -> Result<Option<String>> {
            self.vm_action(&vmm::api::VmResize, vm_resize_data).await
    }

    pub async fn vm_pause(&mut self) -> Result<()> {
        self.vm_action(&vmm::api::VmPause, ()).await.map(|_| ())
    }

    pub async fn vm_resume(&mut self) -> Result<()> {
        self.vm_action(&vmm::api::VmResume, ()).await.map(|_| ())
    }

    pub async fn vm_snapshot(&mut self, snapshot_config: VmSnapshotConfig) -> Result<()> {
        self.vm_action(&vmm::api::VmSnapshot, snapshot_config).await.map(|_| ())
    }

    pub async fn vm_restore(&mut self, restore_config: RestoreConfig) -> Result<()> {
        self.vm_action(&vmm::api::VmRestore, restore_config).await.map(|_| ())
    }
}

#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn failed(message: String) -> Self {
        Error { message }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Error: {}", self.message)
    }
}

// Add this implementation to satisfy the StdError trait requirement
impl std::error::Error for Error {}

fn api_error(error: impl std::fmt::Debug + std::fmt::Display) -> Error {
    Error::failed(format!("{error}"))
}
