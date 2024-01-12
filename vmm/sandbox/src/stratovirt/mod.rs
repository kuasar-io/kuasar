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
use qmp::CpuInfo;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    net::UnixStream,
    sync::watch::{channel, Receiver},
    task::spawn_blocking,
    time::sleep,
};
use unshare::Fd;

use self::{
    config::StratoVirtVMConfig,
    devices::{pcie_rootbus::PcieRootBus, rootport::RootPort, PCIE_ROOTBUS_CAPACITY},
    factory::StratoVirtVMFactory,
    hooks::StratoVirtHooks,
};
use crate::{
    args::Args,
    device::{Bus, BusType, DeviceInfo, Slot, SlotStatus},
    impl_recoverable, load_config,
    param::ToCmdLineParams,
    sandbox::KuasarSandboxer,
    stratovirt::{
        config::StratoVirtConfig,
        devices::{
            block::VirtioBlockDevice, rootport::PCIERootPorts, virtio_net::VirtioNetDevice,
            StratoVirtDevice, StratoVirtHotAttachable, DEFAULT_PCIE_BUS,
        },
        qmp_client::QmpClient,
        utils::detect_pid,
        virtiofs::VirtiofsDaemon,
    },
    utils::{read_std, wait_channel, wait_pid},
    vm::{BlockDriver, Pids, VcpuThreads, VM},
};

pub mod config;
mod devices;
pub mod factory;
pub mod hooks;
mod qmp;
mod qmp_client;
mod utils;
mod virtiofs;

pub(crate) const STRATOVIRT_START_TIMEOUT_IN_SEC: u64 = 10;
pub const CONFIG_STRATOVIRT_PATH: &str = "/var/lib/kuasar/config_stratovirt.toml";

// restart recovery is not supported yet,
// so we annotate the StratoVirtVM with Serialize and Deserlize,
// but skip all the fields serialization.
#[derive(Default, Serialize, Deserialize)]
pub struct StratoVirtVM {
    id: String,
    config: StratoVirtConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn StratoVirtDevice + Sync + Send>>,
    #[serde(skip)]
    hot_attached_devices: Vec<Box<dyn StratoVirtHotAttachable + Sync + Send>>,
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
    virtiofs_daemon: Option<VirtiofsDaemon>,
    #[serde(skip)]
    pcie_root_bus: Option<PcieRootBus>,
    #[serde(skip)]
    pcie_root_ports_pool: Option<PCIERootPorts>,
}

#[async_trait]
impl VM for StratoVirtVM {
    async fn start(&mut self) -> Result<u32> {
        // launch virtiofs daemon process
        debug!("start virtiofs daemon process");
        self.start_virtiofs_daemon().await?;

        debug!("start vm {}", self.id);
        let wait_chan = self.launch().await?;
        self.wait_chan = Some(wait_chan);
        let start_time = SystemTime::now();
        loop {
            match self.create_client().await {
                Ok(c) => {
                    self.client = Some(c);
                    break;
                }
                Err(e) => {
                    trace!("failed to create stratovirt: {:?}", e);
                    if start_time.elapsed().unwrap().as_secs() > STRATOVIRT_START_TIMEOUT_IN_SEC {
                        error!("failed to create stratovirt: {:?}", e);
                        return Err(anyhow!("timeout connect stratovirt, {}", e).into());
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
        if let Some(virtiofsd) = &self.virtiofs_daemon {
            if let Some(pid) = virtiofsd.pid {
                self.pids.affilicated_pids.push(pid);
            }
        }

        Ok(vmm_pid)
    }

    async fn stop(&mut self, force: bool) -> Result<()> {
        // before stop the vm process, stop the virtiofs daemon process firstly
        debug!("stop virtiofs daemon process");
        self.virtiofs_daemon.as_mut().unwrap().stop()?;

        debug!("stop vm {}", self.id);
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
                    if pid == 0 {
                        return Ok(());
                    }
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
            DeviceInfo::Tap(tap_info) => {
                let mut fd_ints = vec![];
                for fd in tap_info.fds {
                    let index = self.append_fd(fd);
                    fd_ints.push(index as i32);
                }

                let virtio_net_device = VirtioNetDevice::new()
                    .id(&tap_info.id)
                    .name(&tap_info.name)
                    .mac_address(&tap_info.mac_address)
                    .transport(self.config.machine.transport())
                    .fds(fd_ints)
                    .vhost(false)
                    .vhostfds(vec![])
                    .bus(Some(DEFAULT_PCIE_BUS.to_string()))
                    .build();
                self.attach_to_bus(virtio_net_device)?;
            }
            _ => {
                todo!()
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
                    "",
                    Some(blk_info.path),
                    Some(blk_info.read_only),
                );
                let index = self.hot_attach_device(device).await?;
                let addr = format!("0000:00:{:02x}.0", index);
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
            DeviceInfo::Char(_char_info) => Err(Error::Unimplemented(
                "hot attach for char device".to_string(),
            )),
        }
    }

    async fn hot_detach(&mut self, _id: &str) -> Result<()> {
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
        let client = self.get_client()?;
        let result = client.execute(qmp::query_cpus {}).await?;
        let mut vcpu_threads_map: HashMap<i64, i64> = HashMap::new();
        for vcpu_info in result.iter() {
            match vcpu_info {
                CpuInfo::Arm { base, .. } | CpuInfo::x86 { base, .. } => {
                    vcpu_threads_map.insert(base.CPU, base.thread_id);
                }
            }
        }
        Ok(VcpuThreads {
            vcpus: vcpu_threads_map,
        })
    }

    fn pids(&self) -> Pids {
        self.pids.clone()
    }
}

impl StratoVirtVM {
    pub fn new(id: &str, netns: &str, base_dir: &str) -> Self {
        Self {
            id: id.to_string(),
            config: StratoVirtConfig::default(),
            devices: vec![],
            hot_attached_devices: vec![],
            fds: vec![],
            console_socket: format!("{}/console.sock", base_dir),
            agent_socket: "".to_string(),
            netns: netns.to_string(),
            block_driver: Default::default(),
            wait_chan: None,
            client: None,
            virtiofs_daemon: None,
            pcie_root_ports_pool: None,
            pcie_root_bus: None,
            pids: Pids::default(),
        }
    }

    fn attach_device<T: StratoVirtDevice + Sync + Send + 'static>(&mut self, device: T) {
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
            "stratovirt startup param: {} {}",
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
                .map_err(|e| anyhow!("failed to spawn stratovirt command: {}", e))?;
            // we set the cmd stdout to pipe, so c.stdout must be Some(pipe);
            tokio::spawn(async move {
                let async_pipe = unsafe { tokio::fs::File::from_raw_fd(pipe_reader.as_raw_fd()) };
                read_std(async_pipe, "stratovirt").await.unwrap_or_default();
            });
            match child.wait() {
                Ok(r) => {
                    if !r.success() {
                        return Err(anyhow!(
                            "stratovirt command execute failed, exit with status {:?}",
                            r
                        )
                        .into());
                    }
                    Ok(())
                }
                Err(e) => {
                    Err(anyhow!("failed to execute stratovirt command with error {}", e).into())
                }
            }
        })
        .await
        .map_err(|e| anyhow!("failed to join the stratovirt startup thread {}", e))??;
        let (tx, rx) = channel((0u32, 0i128));
        let _wait_handle = tokio::spawn(async move {
            // because the direct child process is not the actual running stratovirt process,
            // so we have to read pid from the stratovirt.pid file,
            // NOTE: it is hard to eliminate the race condition when pid reused.
            if let Ok(pid) = detect_pid(&pid_file, &path).await {
                let wait_result = wait_pid(pid as i32).await;
                tx.send(wait_result).unwrap_or_default();
            } else {
                warn!("failed to get stratovirt pid from {}", pid_file);
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

    async fn hot_attach_device<T: StratoVirtHotAttachable + Sync + Send + 'static>(
        &mut self,
        device: T,
    ) -> Result<usize> {
        let (rp_id, rp_index) = self.get_empty_rootport_slot(device.id())?;
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("qmp client is not init"))?;
        device.execute_hot_attach(client, &rp_id).await?;
        self.hot_attached_devices.push(Box::new(device));
        Ok(rp_index)
    }

    fn get_empty_pcie_slot_index(&mut self) -> Result<usize> {
        for (index, slot) in self
            .pcie_root_bus
            .as_mut()
            .unwrap()
            .bus
            .slots
            .iter_mut()
            .enumerate()
        {
            if let SlotStatus::Empty = slot.status {
                return Ok(index);
            }
        }

        Err(Error::ResourceExhausted(
            "slots of pcie.0 root bus is full".to_string(),
        ))
    }

    fn attach_to_bus<T: StratoVirtDevice + Sync + Send + 'static>(
        &mut self,
        mut device: T,
    ) -> Result<()> {
        if self.pcie_root_bus.is_some() {
            // get the empty slot index
            let slot_index = self.get_empty_pcie_slot_index()?;
            // set the pcie slot status to Occupied
            self.pcie_root_bus.as_mut().unwrap().bus.slots[slot_index].status =
                SlotStatus::Occupied(device.id());
            device.set_device_addr(slot_index);
        }
        self.devices.push(Box::new(device));
        Ok(())
    }

    fn create_pcie_root_ports(&mut self, cap_size: usize) -> Result<()> {
        let mut root_ports = vec![];
        let first_empty_slot_index = self.get_empty_pcie_slot_index()?;
        if (PCIE_ROOTBUS_CAPACITY - first_empty_slot_index) < cap_size {
            return Err(Error::ResourceExhausted(format!(
                "left pcie slots number: {}, doesn't meet the require size of PCIE RootPorts: {}",
                (PCIE_ROOTBUS_CAPACITY - first_empty_slot_index),
                cap_size
            )));
        }

        for i in 1..=cap_size {
            let slot_index = first_empty_slot_index + i - 1;
            let root_port = RootPort::new(
                &format!("pcie.{}", i),
                slot_index,
                slot_index,
                DEFAULT_PCIE_BUS,
                None,
            );
            root_ports.push(root_port.clone());
            self.attach_to_bus(root_port)?;
        }

        self.pcie_root_ports_pool = Some(PCIERootPorts {
            id: "root-ports-pool".to_string(),
            bus: Bus {
                r#type: BusType::PCIE,
                id: "pcie-root-ports".to_string(),
                bus_addr: first_empty_slot_index.to_string(),
                slots: vec![Slot::default(); cap_size],
            },
            root_ports,
        });

        Ok(())
    }

    fn get_empty_rootport_slot(&mut self, device_id: String) -> Result<(String, usize)> {
        for rp in self
            .pcie_root_ports_pool
            .as_mut()
            .unwrap()
            .root_ports
            .iter_mut()
        {
            if rp.device_id.is_empty() {
                rp.device_id = device_id;
                return Ok((rp.id.clone(), rp.index));
            }
        }

        Err(Error::ResourceExhausted("slot of rootport".to_string()))
    }

    fn create_vitiofs_daemon(&mut self, daemon_path: &str, base_dir: &str, shared_path: &str) {
        self.virtiofs_daemon = Some(VirtiofsDaemon {
            path: daemon_path.to_string(),
            log_path: format!("{}/virtiofs.log", base_dir),
            socket_path: format!("{}/virtiofs.sock", base_dir),
            shared_dir: shared_path.to_string(),
            pid: None,
        });
    }

    async fn start_virtiofs_daemon(&mut self) -> Result<()> {
        self.virtiofs_daemon
            .as_mut()
            .unwrap()
            .start()
            .map_err(|e| Error::Other(anyhow!("start virtiofs daemon process failed: {}", e)))
    }
}

impl_recoverable!(StratoVirtVM);

pub async fn init_stratovirt_sandboxer(
    args: &Args,
) -> Result<KuasarSandboxer<StratoVirtVMFactory, StratoVirtHooks>> {
    let (config, persist_dir_path) =
        load_config::<StratoVirtVMConfig>(args, CONFIG_STRATOVIRT_PATH).await?;
    let hooks = StratoVirtHooks::new(config.hypervisor.clone());
    let mut s = KuasarSandboxer::new(config.sandbox, config.hypervisor, hooks);
    if !persist_dir_path.is_empty() {
        s.recover(&persist_dir_path).await?;
    }
    Ok(s)
}
