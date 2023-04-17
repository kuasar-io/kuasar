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
use containerd_sandbox::error::{Error, Result};
use log::{debug, error};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    process::Child,
    sync::watch::{channel, Receiver, Sender},
    task::JoinHandle,
};

use crate::{
    cloud_hypervisor::{
        client::ChClient,
        config::{CloudHypervisorConfig, CloudHypervisorVMConfig, VirtiofsdConfig},
        devices::{block::Disk, virtio_net::VirtioNetDevice, CloudHypervisorDevice},
    },
    device::{BusType, DeviceInfo},
    param::ToCmdLineParams,
    utils::{read_file, read_std, set_cmd_fd, set_cmd_netns, wait_pid, write_file_atomic},
    vm::{Recoverable, VM},
};

mod client;
pub mod config;
pub mod devices;
pub mod factory;
pub mod hooks;

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
        virtiofsd_config.shared_dir = base_dir.to_string();
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
        }
    }

    pub fn add_device(&mut self, device: impl CloudHypervisorDevice + Sync + Send + 'static) {
        self.devices.push(Box::new(device));
    }

    async fn pid(&self) -> Result<u32> {
        let pid_file = format!("{}/pid", self.base_dir);
        let pid = read_file(&*pid_file).await.and_then(|x| {
            x.parse::<u32>()
                .map_err(|e| anyhow!("failed to parse pid file {}, {}", x, e).into())
        })?;
        Ok(pid)
    }

    fn get_client(&mut self) -> Result<&mut ChClient> {
        self.client.as_mut().ok_or(Error::NotFound(
            "cloud hypervisor client not inited".to_string(),
        ))
    }

    fn start_virtiofsd(&self) -> Result<()> {
        let params = self.virtiofsd_config.to_cmdline_params("--");
        let mut cmd = tokio::process::Command::new(&self.virtiofsd_config.path);
        cmd.args(params.as_slice());
        debug!("start virtiofsd with cmdline: {:?}", cmd);
        set_cmd_netns(&mut cmd, &self.netns)?;
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        spawn_wait(child, "virtiofsd".to_string(), None, None);
        Ok(())
    }

    fn append_fd(&mut self, fd: RawFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }
}

#[async_trait::async_trait]
impl VM for CloudHypervisorVM {
    async fn start(&mut self) -> Result<u32> {
        debug!("start vm {}", self.id);
        create_dir_all(&self.base_dir).await?;
        self.start_virtiofsd()?;
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
        set_cmd_netns(&mut cmd, &self.netns)?;
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        debug!("start cloud hypervisor with cmdline: {:?}", cmd);
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn cloud hypervisor command: {}", e))?;
        let pid = child.id();
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = tokio::sync::watch::channel((0u32, 0i128));
        spawn_wait(
            child,
            "cloud-hypervisor".to_string(),
            Some(pid_file),
            Some(tx),
        );
        self.client = Some(ChClient::new(&self.config.api_socket)?);
        self.wait_chan = Some(rx);
        Ok(pid.unwrap_or_default())
    }

    async fn stop(&mut self, force: bool) -> Result<()> {
        let pid = self.pid().await?;
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
            DeviceInfo::Physical(_vfio_info) => {
                todo!()
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
        return self.wait_chan.clone();
    }
}

#[async_trait::async_trait]
impl Recoverable for CloudHypervisorVM {
    async fn recover(&mut self) -> Result<()> {
        self.client = Some(ChClient::new(&self.config.api_socket)?);
        let pid = self.pid().await?;
        let (tx, rx) = channel((0u32, 0i128));
        tokio::spawn(async move {
            let wait_result = wait_pid(pid as i32).await;
            tx.send(wait_result).unwrap_or_default();
        });
        self.wait_chan = Some(rx);
        Ok(())
    }
}

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
