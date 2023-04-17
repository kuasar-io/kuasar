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

use async_trait::async_trait;
use containerd_sandbox::error::Result;

use crate::{
    device::{Bus, BusType, Device, Slot},
    param::ToCmdLineParams,
    qemu::{devices::bridge::Bridge, qmp_client::QmpClient},
};

pub mod block;
pub mod bridge;
pub mod char;
pub mod scsi;
pub mod serial;
pub mod vfio;
pub mod vhost_user;
pub mod virtio_9p;
pub mod virtio_net;
pub mod virtio_rng;
pub mod vsock;

pub(crate) const PCI_BRIDGE_CAPACITY: usize = 30;

pub(crate) const BRIDGE_PCI_START_ADDRESS: u32 = 2;

pub trait QemuDevice: Device + ToCmdLineParams {}

impl<T> QemuDevice for T where T: Device + ToCmdLineParams {}

#[async_trait]
pub trait HotAttachable {
    async fn execute_hot_attach(
        &self,
        client: &QmpClient,
        bus_type: &BusType,
        bus_id: &str,
        slot_index: usize,
    ) -> Result<()>;
    async fn execute_hot_detach(&self, client: &QmpClient) -> Result<()>;
}

pub trait QemuHotAttachable: Device + HotAttachable {}

impl<T> QemuHotAttachable for T where T: Device + HotAttachable {}

pub fn create_bridges(count: u32, machine_type: &str) -> Vec<Bridge> {
    let (r#type, driver, bus) = match machine_type {
        crate::qemu::config::MACHINE_TYPE_VIRT => ("pcie", "pcie-pci-bridge", "pcie.0"),
        _ => ("pci", "pci-bridge", "pci.0"),
    };
    let mut bridges = vec![];
    for i in 0..count {
        let addr = BRIDGE_PCI_START_ADDRESS + i;
        let b = Bridge {
            driver: driver.to_string(),
            bus_name: bus.to_string(),
            id: format!("{}-bridge-{}", r#type, i),
            chassis: i + 1,
            shpc: true,
            addr: Some(format!("{:x}", addr)),
            romfile: None,
            bus: Bus {
                r#type: BusType::PCI,
                id: format!("{}-bridge-{}", r#type, i),
                bus_addr: format!("{:02x}", addr),
                slots: vec![Slot::default(); PCI_BRIDGE_CAPACITY],
            },
        };
        bridges.push(b);
    }
    bridges
}
