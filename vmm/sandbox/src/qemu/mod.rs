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

use std::{
    collections::HashMap,
    os::unix::io::{AsRawFd, FromRawFd, RawFd},
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::{Error, Result};
use futures_util::TryFutureExt;
use log::{debug, error, trace, warn};
use nix::{fcntl::OFlag, libc::kill, sys::stat::Mode};
use qapi::qmp::quit;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    net::UnixStream,
    sync::watch::{channel, Receiver},
    task::spawn_blocking,
    time::sleep,
};
use unshare::Fd;

use self::{factory::QemuVMFactory, hooks::QemuHooks};
use crate::{
    args::Args,
    device::{BusType, DeviceInfo, SlotStatus, Transport},
    impl_recoverable,
    kata_config::KataConfig,
    param::ToCmdLineParams,
    qemu::{
        config::QemuConfig,
        devices::{
            block::{VirtioBlockDevice, VIRTIO_BLK_DRIVER},
            char::{CharDevice, VIRT_SERIAL_PORT_DRIVER},
            vfio::VfioDevice,
            vhost_user::{VhostUserDevice, VhostUserType},
            virtio_net::VirtioNetDevice,
            QemuDevice, QemuHotAttachable,
        },
        qmp_client::QmpClient,
        utils::detect_pid,
    },
    sandbox::KuasarSandboxer,
    utils::{read_std, wait_channel, wait_pid},
    vm::{BlockDriver, Pids, VcpuThreads, VM},
};

pub mod config;
mod devices;
pub mod factory;
pub mod hooks;
mod qmp_client;
mod utils;

pub(crate) const QEMU_START_TIMEOUT_IN_SEC: u64 = 10;

// restart recovery is not supported yet,
// so we annotate the QemuVM with Serialize and Deserlize,
// but skip all the fields serialization.
#[derive(Default, Serialize, Deserialize)]
pub struct QemuVM {
    id: String,
    config: QemuConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn QemuDevice + Sync + Send>>,
    #[serde(skip)]
    hot_attached_devices: Vec<Box<dyn QemuHotAttachable + Sync + Send>>,
    fds: Vec<RawFd>,
    console_socket: String,
    agent_socket: String,
    netns: String,
    pids: Pids,
    #[serde(skip)]
    block_driver: BlockDriver,
    #[serde(skip)]
    wait_chan: Option<Receiver<(u32, i128)>>,
    #[serde(skip)]
    client: Option<QmpClient>,
}

#[async_trait]
impl VM for QemuVM {
    async fn start(&mut self) -> Result<u32> {
        debug!("start vm {}", self.id);
        let wait_chan = self.launch().await?;
        self.wait_chan = Some(wait_chan);
        // close the fds after launch qemu
        self.fds = vec![];
        let start_time = SystemTime::now();
        loop {
            match self.create_client().await {
                Ok(c) => {
                    self.client = Some(c);
                    break;
                }
                Err(e) => {
                    trace!("failed to create qmp: {:?}", e);
                    if start_time.elapsed().unwrap().as_secs() > QEMU_START_TIMEOUT_IN_SEC {
                        error!("failed to create qmp: {:?}", e);
                        return Err(anyhow!("timeout connect qmp, {}", e).into());
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            }
        }

        let console_socket = self.console_socket.clone();
        tokio::spawn(async move {
            UnixStream::connect(&*console_socket)
                .map_err(|e| e.into())
                .and_then(|s| read_std(s, "console"))
                .await
                .unwrap_or_else(|e| {
                    error!("failed to read console log, {}", e);
                });
        });
        // update vmm related pids
        let vmm_pid = detect_pid(self.config.pid_file.as_str(), self.config.path.as_str()).await?;
        self.pids.vmm_pid = Some(vmm_pid);
        Ok(0)
    }

    async fn stop(&mut self, force: bool) -> Result<()> {
        if !force {
            let client = self.get_client()?;
            client.execute(quit {}).await?;
        } else {
            let client_res = self.get_client();
            if let Ok(client) = client_res {
                let _ = client.execute(quit {}).await;
            }
        }

        if let Err(e) = self.wait_stop(Duration::from_secs(10)).await {
            if force {
                if let Ok(pid) = self.pid() {
                    unsafe { kill(pid as i32, 9) };
                }
            } else {
                return Err(e);
            }
        }

        Ok(())
    }

    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = VirtioBlockDevice::new(
                    &Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
                    &blk_info.id,
                    Some(blk_info.path),
                    blk_info.read_only,
                );
                self.attach_device(device);
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
                    Transport::Pci,
                    fd_ints,
                    vec![],
                );
                self.attach_device(device);
            }
            DeviceInfo::Physical(vfio_info) => {
                let device = VfioDevice::new(&vfio_info.id, &vfio_info.bdf);
                self.attach_device(device);
            }
            DeviceInfo::VhostUser(vhost_user_info) => {
                let device = VhostUserDevice::new(
                    &vhost_user_info.id,
                    VhostUserType::VhostUserNet(vhost_user_info.r#type),
                    &vhost_user_info.socket_path,
                    &vhost_user_info.mac_address,
                );
                self.attach_device(device);
            }
            DeviceInfo::Char(char_info) => {
                let device = CharDevice::new_with_backend_type(
                    char_info.backend.clone(),
                    &char_info.id,
                    &char_info.chardev_id,
                    VIRT_SERIAL_PORT_DRIVER,
                    Some(char_info.name.clone()),
                );
                self.attach_device(device);
            }
        };
        Ok(())
    }

    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = VirtioBlockDevice::new(
                    "",
                    &blk_info.id,
                    Some(blk_info.path),
                    blk_info.read_only,
                );
                let (bus_addr, index) = self
                    .hot_attach_device(device, self.block_driver.to_bus_type())
                    .await?;
                let addr = match self.block_driver {
                    BlockDriver::VirtioBlk => {
                        format!("0000:{}:{:02x}.0", bus_addr, index)
                    }
                    BlockDriver::VirtioMmio => {
                        return Err(Error::Unimplemented(
                            "virtio-mmio not implemented yet".to_string(),
                        ));
                    }
                    BlockDriver::VirtioScsi => {
                        format!("{}:{}", index / 256, index % 256)
                    }
                };
                Ok((self.block_driver.to_bus_type(), addr))
            }
            DeviceInfo::Tap(_tap_info) => Err(Error::Unimplemented(
                "hot attach for tap device".to_string(),
            )),
            DeviceInfo::Physical(_vfio_info) => Err(Error::Unimplemented(
                "hot attach for vfio device".to_string(),
            )),
            DeviceInfo::VhostUser(_vhost_user_info) => Err(Error::Unimplemented(
                "hot attach for vhost_user device".to_string(),
            )),
            DeviceInfo::Char(char_info) => {
                let device = CharDevice::new_with_backend_type(
                    char_info.backend.clone(),
                    &char_info.id,
                    &char_info.chardev_id,
                    VIRT_SERIAL_PORT_DRIVER,
                    Some(char_info.name.clone()),
                );
                self.hot_attach_device(device, BusType::SERIAL).await?;
                // address is not import for char devices as guest will find the device by the name
                Ok((BusType::PCI, char_info.name.clone()))
            }
        }
    }

    async fn hot_detach(&mut self, id: &str) -> Result<()> {
        let index = self.hot_attached_devices.iter().position(|x| x.id() == id);
        let device = match index {
            None => {
                return Ok(());
            }
            Some(index) => self.hot_attached_devices.remove(index),
        };

        let client = match self.get_client() {
            Ok(c) => c,
            Err(e) => {
                // rollback, add it back to the list
                self.hot_attached_devices.push(device);
                return Err(e);
            }
        };

        if let Err(e) = device.execute_hot_detach(client).await {
            // rollback, add it back to the list
            self.hot_attached_devices.push(device);
            return Err(e);
        }
        self.detach_from_bus(id);
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        let client = self.get_client()?;
        let _res = client.execute(qapi::qmp::query_status {}).await?;
        Ok(())
    }

    fn socket_address(&self) -> String {
        self.agent_socket.to_string()
    }

    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>> {
        self.wait_chan.clone()
    }

    async fn vcpus(&self) -> Result<VcpuThreads> {
        // TODO: support get vcpu threads id
        Ok(VcpuThreads {
            vcpus: HashMap::new(),
        })
    }

    fn pids(&self) -> Pids {
        // TODO: support get all vmm related pids
        Pids::default()
    }
}

impl QemuVM {
    pub fn new(id: &str, netns: &str, base_dir: &str) -> Self {
        Self {
            id: id.to_string(),
            config: QemuConfig::default(),
            devices: vec![],
            hot_attached_devices: vec![],
            fds: vec![],
            console_socket: format!("{}/console.sock", base_dir),
            agent_socket: "".to_string(),
            netns: netns.to_string(),
            pids: Pids::default(),
            block_driver: Default::default(),
            wait_chan: None,
            client: None,
        }
    }

    fn attach_device<T: QemuDevice + Sync + Send + 'static>(&mut self, device: T) {
        self.devices.push(Box::new(device));
    }

    fn append_fd(&mut self, fd: RawFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }

    async fn launch(&self) -> Result<Receiver<(u32, i128)>> {
        let mut params = self.config.to_cmdline_params("-");
        for d in self.devices.iter() {
            params.extend(d.to_cmdline_params("-"));
        }
        let fds = self.fds.to_vec();
        let path = self.config.path.to_string();
        // pid file should not be empty
        let pid_file = self.config.pid_file.to_string();
        debug!(
            "qemu startup param: {} {}",
            &self.config.path,
            params.join(" ")
        );
        let (pipe_reader, pipe_writer) = os_pipe::pipe()?;
        let netns = self.netns.to_string();
        let path_clone = path.clone();
        spawn_blocking(move || -> Result<()> {
            let mut cmd = unshare::Command::new(&*path_clone);
            cmd.args(params.as_slice());
            for (i, &x) in fds.iter().enumerate() {
                cmd.file_descriptor(
                    (3 + i) as RawFd,
                    Fd::from_file(unsafe { std::fs::File::from_raw_fd(x) }),
                );
            }
            if !netns.is_empty() {
                let netns_fd = nix::fcntl::open(&*netns, OFlag::O_CLOEXEC, Mode::empty())
                    .map_err(|e| anyhow!("failed to open netns {}", e))?;
                cmd.set_namespace(&netns_fd, unshare::Namespace::Net)?;
            }
            let pipe_writer2 = pipe_writer.try_clone()?;
            cmd.stdout(unshare::Stdio::from_file(pipe_writer));
            cmd.stderr(unshare::Stdio::from_file(pipe_writer2));
            let mut child = cmd
                .spawn()
                .map_err(|e| anyhow!("failed to spawn qemu command: {}", e))?;
            // we set the cmd stdout to pipe, so c.stdout must be Some(pipe);
            tokio::spawn(async move {
                let async_pipe = unsafe { tokio::fs::File::from_raw_fd(pipe_reader.as_raw_fd()) };
                read_std(async_pipe, "qemu").await.unwrap_or_default();
            });
            match child.wait() {
                Ok(r) => {
                    if !r.success() {
                        return Err(anyhow!(
                            "qemu command execute failed, exit with status {:?}",
                            r
                        )
                        .into());
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow!("failed to execute qemu command with error {}", e).into()),
            }
        })
        .await
        .map_err(|e| anyhow!("failed to join the qemu startup thread {}", e))??;
        let (tx, rx) = channel((0u32, 0i128));
        let _wait_handle = tokio::spawn(async move {
            // because the direct child process is not the actual running qemu process,
            // so we have to read pid from the qemu.pid file,
            // NOTE: it is hard to eliminate the race condition when pid reused.
            if let Ok(pid) = detect_pid(&pid_file, &path).await {
                let wait_result = wait_pid(pid as i32).await;
                tx.send(wait_result).unwrap_or_default();
            } else {
                warn!("failed to get qemu pid from {}", pid_file);
                tx.send((0, OffsetDateTime::now_utc().unix_timestamp_nanos()))
                    .unwrap_or_default();
            }
        });
        Ok(rx)
    }

    async fn create_client(&self) -> Result<QmpClient> {
        let socket_addr = self
            .config
            .qmp_socket
            .as_ref()
            .map(|x| x.name.to_string())
            .ok_or_else(|| anyhow!("failed to get qmp socket path"))?;
        QmpClient::new(&socket_addr).await
    }

    fn get_client(&self) -> Result<&QmpClient> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("qmp client is not init"))?;
        Ok(client)
    }

    async fn wait_stop(&mut self, t: Duration) -> Result<()> {
        if let Some(rx) = self.wait_chan.clone() {
            let (_, ts) = *rx.borrow();
            if ts == 0 {
                wait_channel(t, rx).await?;
            }
        }
        Ok(())
    }

    fn pid(&self) -> Result<u32> {
        match self.pids.vmm_pid {
            None => Err(anyhow!("empty pid from vmm_pid").into()),
            Some(pid) => Ok(pid),
        }
    }

    async fn hot_attach_device<T: QemuHotAttachable + Sync + Send + 'static>(
        &mut self,
        device: T,
        bus_type: BusType,
    ) -> Result<(String, usize)> {
        let (bus_id, index) = self.empty_slot(bus_type.clone())?;
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("qmp client is not init"))?;
        device
            .execute_hot_attach(client, &bus_type, &bus_id, index)
            .await?;
        let (bus_addr, index) = match self.attach_to_bus(&bus_id, index, &device.id()) {
            Ok((addr, index)) => (addr, index),
            Err(e) => {
                self.hot_detach(&device.id()).await?;
                return Err(e);
            }
        };
        self.hot_attached_devices.push(Box::new(device));
        Ok((bus_addr, index))
    }

    fn empty_slot(&mut self, bus_type: BusType) -> Result<(String, usize)> {
        for b in self
            .devices
            .iter_mut()
            .filter_map(|x| x.bus().filter(|b| b.r#type == bus_type))
        {
            let res = b.empty_slot();
            if let Some(index) = res {
                return Ok((b.id.to_string(), index));
            }
        }
        Err(Error::ResourceExhausted(format!("slot of {:?}", bus_type)))
    }

    fn attach_to_bus(
        &mut self,
        bus_id: &str,
        index: usize,
        device_id: &str,
    ) -> Result<(String, usize)> {
        let bus = self
            .devices
            .iter_mut()
            .filter_map(|x| x.bus())
            .find(|b| b.id == bus_id)
            .ok_or_else(|| anyhow!("can not get bus by id {}", bus_id))?;
        if let Some(s) = bus.slots.get_mut(index) {
            s.status = SlotStatus::Occupied(device_id.to_string());
        }
        Ok((bus.bus_addr.to_string(), index))
    }

    fn detach_from_bus(&mut self, device_id: &str) {
        self.devices
            .iter_mut()
            .filter_map(|x| x.bus())
            .for_each(|b| {
                if let Some(x) = b.slots.iter_mut().find(|s| {
                    if let SlotStatus::Occupied(id) = &s.status {
                        if id == device_id {
                            return true;
                        }
                    }
                    false
                }) {
                    x.status = SlotStatus::Empty;
                }
            });
    }
}

impl_recoverable!(QemuVM);

pub async fn init_qemu_sandboxer(args: &Args) -> Result<KuasarSandboxer<QemuVMFactory, QemuHooks>> {
    // For compatibility with kata config
    let config_path = std::env::var("KATA_CONFIG_PATH")
        .unwrap_or_else(|_| "/usr/share/defaults/kata-containers/configuration.toml".to_string());

    let path = std::path::Path::new(&config_path);
    if path.exists() {
        KataConfig::init(path).await?;
    }

    let vmm_config = KataConfig::hypervisor_config("qemu", |h| h.clone()).await?;
    let vmm_config = vmm_config.to_qemu_config()?;
    let sandbox_config = KataConfig::sandbox_config("qemu").await?;
    let hooks = QemuHooks::new(vmm_config.clone());
    let mut s = KuasarSandboxer::new(sandbox_config, vmm_config, hooks);

    // Check for "--dir" argument and recover from persisted directory
    if let Some(persist_dir_path) = &args.dir {
        if std::path::Path::new(&persist_dir_path).exists() {
            s.recover(persist_dir_path).await?;
        }
    }

    Ok(s)
}
