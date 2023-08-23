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

use std::{collections::HashMap, str::FromStr};

use async_trait::async_trait;
use containerd_sandbox::{
    error::{Error, Result},
    SandboxOption,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch::Receiver;

use crate::{
    device::{BusType, DeviceInfo},
    sandbox::KuasarSandbox,
};

const VIRTIO_FS: &str = "virtio-fs";
const VIRTIO_9P: &str = "virtio-9p";

#[async_trait]
pub trait VMFactory {
    type VM: VM + Sync + Send;
    type Config: Sync + Send;
    fn new(config: Self::Config) -> Self;
    async fn create_vm(&self, id: &str, s: &SandboxOption) -> Result<Self::VM>;
}

#[async_trait]
pub trait Hooks<V: VM + Sync + Send> {
    async fn post_create(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait VM: Serialize + Sync + Send {
    async fn start(&mut self) -> Result<u32>;
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()>;
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)>;
    async fn hot_detach(&mut self, id: &str) -> Result<()>;
    async fn ping(&self) -> Result<()>;
    fn socket_address(&self) -> String;
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>>;
    async fn vcpus(&self) -> Result<VcpuThreads>;
    fn pids(&self) -> Pids;
}

#[macro_export]
macro_rules! impl_recoverable {
    ($ty:ty) => {
        #[async_trait]
        impl $crate::vm::Recoverable for $ty {
            async fn recover(&mut self) -> Result<()> {
                self.client = Some(self.create_client().await?);
                let pid = self.pid()?;
                let (tx, rx) = channel((0u32, 0i128));
                tokio::spawn(async move {
                    let wait_result = wait_pid(pid as i32).await;
                    tx.send(wait_result).unwrap_or_default();
                });
                self.wait_chan = Some(rx);
                Ok(())
            }
        }
    };
}

#[async_trait]
pub trait Recoverable {
    async fn recover(&mut self) -> Result<()>;
}

#[derive(Deserialize, Debug, Clone)]
pub struct HypervisorCommonConfig {
    #[serde(default)]
    pub debug: bool,
    pub vcpus: u32,
    pub memory_in_mb: u32,
    #[serde(default)]
    pub kernel_path: String,
    #[serde(default)]
    pub image_path: String,
    #[serde(default)]
    pub initrd_path: String,
    #[serde(default)]
    pub kernel_params: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub enable_mem_prealloc: bool,
}

impl Default for HypervisorCommonConfig {
    fn default() -> Self {
        Self {
            debug: false,
            vcpus: 1,
            memory_in_mb: 1024,
            kernel_path: "/var/lib/kuasar/vmlinux.bin".to_string(),
            image_path: "".to_string(),
            initrd_path: "".to_string(),
            kernel_params: "".to_string(),
            firmware: "".to_string(),
            enable_mem_prealloc: false,
        }
    }
}

#[derive(Clone, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum BlockDriver {
    VirtioBlk,
    VirtioScsi,
    VirtioMmio,
}

impl Default for BlockDriver {
    fn default() -> Self {
        Self::VirtioBlk
    }
}

impl BlockDriver {
    pub fn from(s: &str) -> Self {
        match s {
            "virtio-blk" => Self::VirtioBlk,
            "virtio-scsi" => Self::VirtioScsi,
            "virtio-mmio" => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }

    pub fn to_driver_string(&self) -> String {
        match self {
            BlockDriver::VirtioBlk => "blk".to_string(),
            BlockDriver::VirtioMmio => "mmioblk".to_string(),
            BlockDriver::VirtioScsi => "scsi".to_string(),
        }
    }

    pub fn to_bus_type(&self) -> BusType {
        match self {
            BlockDriver::VirtioBlk => BusType::PCI,
            BlockDriver::VirtioMmio => BusType::NULL,
            BlockDriver::VirtioScsi => BusType::SCSI,
        }
    }

    pub fn from_bus_type(bus_type: &BusType) -> Self {
        match bus_type {
            BusType::PCI => Self::VirtioBlk,
            BusType::SCSI => Self::VirtioScsi,
            BusType::MMIO => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ShareFsType {
    Virtio9P,
    VirtioFS,
}

impl FromStr for ShareFsType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            VIRTIO_FS => Ok(ShareFsType::VirtioFS),
            VIRTIO_9P => Ok(ShareFsType::Virtio9P),
            _ => Err(Error::InvalidArgument(s.to_string())),
        }
    }
}

#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Pids {
    pub vmm_pid: Option<u32>,
    pub affilicated_pids: Vec<u32>,
}
