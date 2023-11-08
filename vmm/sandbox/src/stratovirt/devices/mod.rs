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

use self::{device::GetAndSetDeviceAddr, pcie_rootbus::PcieRootBus};
use crate::{
    device::{Bus, BusType, Device, Slot, SlotStatus},
    param::ToCmdLineParams,
    stratovirt::qmp_client::QmpClient,
};

#[macro_use]
pub mod device;

pub mod block;
pub mod char;
pub mod console;
pub mod pcie_rootbus;
pub mod rng;
pub mod rootport;
pub mod serial;
pub mod vhost_user_fs;
pub mod virtio_net;
pub mod vsock;

pub(crate) const PCIE_ROOTPORT_CAPACITY: usize = 10;
pub(crate) const PCIE_ROOTBUS_CAPACITY: usize = 32;

pub(crate) const DEFAULT_PCIE_BUS: &str = "pcie.0";
pub(crate) const DEFAULT_RNG_DEVICE_ID: &str = "rng0";
pub(crate) const DEFAULT_SERIAL_DEVICE_ID: &str = "virtio-serial0";
pub(crate) const DEFAULT_CONSOLE_DEVICE_ID: &str = "virtio-console0";
pub(crate) const DEFAULT_CONSOLE_CHARDEV_ID: &str = "charconsole0";

#[allow(dead_code)]
pub(crate) const VIRTIO_RND_DEVICE_ADDR: usize = 1;
#[allow(dead_code)]
pub(crate) const VIRTIO_SERIAL_CONSOLE_ADDR: usize = 2;
#[allow(dead_code)]
pub(crate) const VHOST_VSOCK_ADDR: usize = 3;
#[allow(dead_code)]
pub(crate) const VHOST_USER_FS_ADDR: usize = 4;
#[allow(dead_code)]
pub(crate) const ROOTPORT_PCI_START_ADDR: usize = 5;

pub trait StratoVirtDevice: Device + ToCmdLineParams + GetAndSetDeviceAddr {}

impl<T> StratoVirtDevice for T where T: Device + ToCmdLineParams + GetAndSetDeviceAddr {}

#[async_trait]
pub trait HotAttachable {
    async fn execute_hot_attach(&self, client: &QmpClient, bus_id: &str) -> Result<()>;
    async fn execute_hot_detach(&self, client: &QmpClient) -> Result<()>;
}

pub trait StratoVirtHotAttachable: Device + HotAttachable {}

impl<T> StratoVirtHotAttachable for T where T: Device + HotAttachable {}

pub fn create_pcie_root_bus() -> Option<PcieRootBus> {
    let mut pcie_root_bus = PcieRootBus {
        id: "pcie.0".to_string(),
        bus: Bus {
            r#type: BusType::PCIE,
            id: "pcie.0".to_string(),
            bus_addr: "".to_string(),
            slots: vec![Slot::default(); PCIE_ROOTBUS_CAPACITY],
        },
    };

    // since pcie.0/0x0 addr is reserved, set slot 0 status is "SlotStatus::Occupied"
    pcie_root_bus.bus.slots[0].status = SlotStatus::Occupied("reserved".to_string());
    Some(pcie_root_bus)
}

#[cfg(test)]
mod tests {
    use super::create_pcie_root_bus;
    use crate::device::SlotStatus;

    #[test]
    fn test_create_pcie_root_bus() {
        let pcie_root_bus = create_pcie_root_bus();
        if let SlotStatus::Occupied(s) = &pcie_root_bus.unwrap().bus.slots[0].status {
            assert_eq!(s, "reserved");
        }
    }
}
