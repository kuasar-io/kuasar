/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{os::unix::io::RawFd, process::Stdio};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::{Error, Result};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    process::Child,
    sync::watch::{channel, Receiver, Sender},
    task::JoinHandle,
};
use vmm_common::SHARED_DIR_SUFFIX;

use self::{factory::CloudHypervisorVMFactory, hooks::CloudHypervisorHooks};
use crate::{
    args::Args,
    cloud_hypervisor::{
        client::ChClient,
        config::{CloudHypervisorConfig, CloudHypervisorVMConfig, VirtiofsdConfig},
        devices::{
            block::Disk, vfio::VfioDevice, virtio_net::VirtioNetDevice, CloudHypervisorDevice,
        },
    },
    device::{BusType, DeviceInfo},
    impl_recoverable, load_config,
    param::ToCmdLineParams,
    sandbox::KuasarSandboxer,
    utils::{read_std, set_cmd_fd, set_cmd_netns, wait_pid, write_file_atomic},
    vm::{Pids, VcpuThreads, VM},
};

mod client;
pub mod config;
pub mod devices;
pub mod factory;
pub mod hooks;

const VCPU_PREFIX: &str = "vcpu";
pub const CONFIG_CLH_PATH: &str = "/var/lib/kuasar/config_clh.toml";

#[derive(Default, Serialize, Deserialize)]
pub struct CloudHypervisorVM {
    id: String,
    config: CloudHypervisorConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn CloudHypervisorDevice + Sync + Send>>,
    netns: String,
    base_dir: String,
    agent_socket: String,
    virtiofsd_config: VirtiofsdConfig,
    #[serde(skip)]
    wait_chan: Option<Receiver<(u32, i128)>>,
    #[serde(skip)]
    client: Option<ChClient>,
    fds: Vec<RawFd>,
    pids: Pids,
}

impl CloudHypervisorVM {
    pub fn new(id: &str, netns: &str, base_dir: &str, vm_config: &CloudHypervisorVMConfig) -> Self {
        let mut config = CloudHypervisorConfig::from(vm_config);
        config.api_socket = format!("{}/api.sock", base_dir);
        if !vm_config.common.initrd_path.is_empty() {
            config.initramfs = Some(vm_config.common.initrd_path.clone());
        }

        let mut virtiofsd_config = vm_config.virtiofsd.clone();
        virtiofsd_config.socket_path = format!("{}/virtiofs.sock", base_dir);
        virtiofsd_config.shared_dir = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        Self {
            id: id.to_string(),
            config,
            devices: vec![],
            netns: netns.to_string(),
            base_dir: base_dir.to_string(),
            agent_socket: "".to_string(),
            virtiofsd_config,
            wait_chan: None,
            client: None,
            fds: vec![],
            pids: Pids::default(),
        }
    }

    pub fn add_device(&mut self, device: impl CloudHypervisorDevice + Sync + Send + 'static) {
        self.devices.push(Box::new(device));
    }

    fn pid(&self) -> Result<u32> {
        match self.pids.vmm_pid {
            None => Err(anyhow!("empty pid from vmm_pid").into()),
            Some(pid) => Ok(pid),
        }
    }

    async fn create_client(&self) -> Result<ChClient> {
        ChClient::new(self.config.api_socket.to_string()).await
    }

    fn get_client(&mut self) -> Result<&mut ChClient> {
        self.client.as_mut().ok_or(Error::NotFound(
            "cloud hypervisor client not inited".to_string(),
        ))
    }

    async fn start_virtiofsd(&self) -> Result<u32> {
        create_dir_all(&self.virtiofsd_config.shared_dir).await?;
        let params = self.virtiofsd_config.to_cmdline_params("--");
        let mut cmd = tokio::process::Command::new(&self.virtiofsd_config.path);
        cmd.args(params.as_slice());
        debug!("start virtiofsd with cmdline: {:?}", cmd);
        set_cmd_netns(&mut cmd, self.netns.to_string())?;
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        let pid = child
            .id()
            .ok_or(anyhow!("the virtiofsd has been polled to completion"))?;
        spawn_wait(child, format!("virtiofsd {}", self.id), None, None);
        Ok(pid)
    }

    fn append_fd(&mut self, fd: RawFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }
}

#[async_trait]
impl VM for CloudHypervisorVM {
    async fn start(&mut self) -> Result<u32> {
        create_dir_all(&self.base_dir).await?;
        let virtiofsd_pid = self.start_virtiofsd().await?;
        let mut params = self.config.to_cmdline_params("--");
        for d in self.devices.iter() {
            params.extend(d.to_cmdline_params("--"));
        }

        // the log level is single hyphen parameter, has to handle separately
        if self.config.debug {
            params.push("-vv".to_string());
        }

        let mut cmd = tokio::process::Command::new(&self.config.path);
        cmd.args(params.as_slice());

        set_cmd_fd(&mut cmd, self.fds.to_vec())?;
        set_cmd_netns(&mut cmd, self.netns.to_string())?;
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        info!("start cloud hypervisor with cmdline: {:?}", cmd);
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn cloud hypervisor command: {}", e))?;
        let pid = child.id();
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = tokio::sync::watch::channel((0u32, 0i128));
        spawn_wait(
            child,
            format!("cloud-hypervisor {}", self.id),
            Some(pid_file),
            Some(tx),
        );
        self.client = Some(self.create_client().await?);
        self.wait_chan = Some(rx);

        // update vmm related pids
        self.pids.vmm_pid = pid;
        self.pids.affilicated_pids.push(virtiofsd_pid);
        // TODO: add child virtiofsd process
        Ok(pid.unwrap_or_default())
    }

    async fn stop(&mut self, force: bool) -> Result<()> {
        let pid = self.pid()?;
        if pid == 0 {
            return Ok(());
        }
        let signal = if force { 9 } else { 15 };
        unsafe { nix::libc::kill(pid as i32, signal) };

        Ok(())
    }

    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = Disk::new(&blk_info.id, &blk_info.path, blk_info.read_only, true);
                self.add_device(device);
            }
            DeviceInfo::Tap(tap_info) => {
                let mut fd_ints = vec![];
                for fd in tap_info.fds {
                    let index = self.append_fd(fd);
                    fd_ints.push(index as i32);
                }
                let device = VirtioNetDevice::new(
                    &tap_info.id,
                    Some(tap_info.name),
                    &tap_info.mac_address,
                    fd_ints,
                );
                self.add_device(device);
            }
            DeviceInfo::Physical(vfio_info) => {
                let device = VfioDevice::new(&vfio_info.id, &vfio_info.bdf);
                self.add_device(device);
            }
            DeviceInfo::VhostUser(_vhost_user_info) => {
                todo!()
            }
            DeviceInfo::Char(_char_info) => {
                unimplemented!()
            }
        };
        Ok(())
    }

    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)> {
        let client = self.get_client()?;
        let addr = client.hot_attach(device_info)?;
        Ok((BusType::PCI, addr))
    }

    async fn hot_detach(&mut self, id: &str) -> Result<()> {
        let client = self.get_client()?;
        client.hot_detach(id)?;
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        // TODO
        Ok(())
    }

    fn socket_address(&self) -> String {
        self.agent_socket.to_string()
    }

    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>> {
        self.wait_chan.clone()
    }

    async fn vcpus(&self) -> Result<VcpuThreads> {
        // Refer to https://github.com/firecracker-microvm/firecracker/issues/718
        Ok(VcpuThreads {
            vcpus: procfs::process::Process::new(self.pid()? as i32)
                .map_err(|e| anyhow!("failed to get process {}", e))?
                .tasks()
                .map_err(|e| anyhow!("failed to get tasks {}", e))?
                .flatten()
                .filter_map(|t| {
                    t.stat()
                        .map_err(|e| anyhow!("failed to get stat {}", e))
                        .ok()?
                        .comm
                        .strip_prefix(VCPU_PREFIX)
                        .and_then(|comm| comm.parse().ok())
                        .map(|index| (index, t.tid as i64))
                })
                .collect(),
        })
    }

    fn pids(&self) -> Pids {
        self.pids.clone()
    }
}

impl_recoverable!(CloudHypervisorVM);

macro_rules! read_stdio {
    ($stdio:expr, $cmd_name:ident) => {
        if let Some(std) = $stdio {
            let cmd_name_clone = $cmd_name.clone();
            tokio::spawn(async move {
                read_std(std, &cmd_name_clone).await.unwrap_or_default();
            });
        }
    };
}

fn spawn_wait(
    child: Child,
    cmd_name: String,
    pid_file_path: Option<String>,
    exit_chan: Option<Sender<(u32, i128)>>,
) -> JoinHandle<()> {
    let mut child = child;
    tokio::spawn(async move {
        if let Some(pid_file) = pid_file_path {
            if let Some(pid) = child.id() {
                write_file_atomic(&pid_file, &pid.to_string())
                    .await
                    .unwrap_or_default();
            }
        }

        read_stdio!(child.stdout.take(), cmd_name);
        read_stdio!(child.stderr.take(), cmd_name);

        match child.wait().await {
            Ok(status) => {
                if !status.success() {
                    error!("{} exit {}", cmd_name, status);
                }
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((
                        status.code().unwrap_or_default() as u32,
                        now.unix_timestamp_nanos(),
                    ))
                    .unwrap_or_default();
                }
            }
            Err(e) => {
                error!("{} wait error {}", cmd_name, e);
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((0, now.unix_timestamp_nanos())).unwrap_or_default();
                }
            }
        }
    })
}

pub async fn init_cloud_hypervisor_sandboxer(
    args: &Args,
) -> Result<KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks>> {
    let (config, persist_dir_path) =
        load_config::<CloudHypervisorVMConfig>(args, CONFIG_CLH_PATH).await?;
    let hooks = CloudHypervisorHooks {};
    let mut s = KuasarSandboxer::new(config.sandbox, config.hypervisor, hooks);
    if !persist_dir_path.is_empty() {
        s.recover(&persist_dir_path).await?;
    }
    Ok(s)
}
