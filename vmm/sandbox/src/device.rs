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

use std::os::unix::io::RawFd;

use containerd_sandbox::error::{Error, Result};

macro_rules! impl_device_no_bus {
    ($ty:ty) => {
        impl crate::device::Device for $ty {
            fn id(&self) -> String {
                self.id.to_string()
            }

            fn bus(&mut self) -> Option<&mut crate::device::Bus> {
                None
            }
        }
    };
}

macro_rules! impl_device {
    ($ty:ty) => {
        impl crate::device::Device for $ty {
            fn id(&self) -> String {
                self.id.to_string()
            }

            fn bus(&mut self) -> Option<&mut crate::device::Bus> {
                Some(&mut self.bus)
            }
        }
    };
}

pub trait Device {
    fn id(&self) -> String;
    fn bus(&mut self) -> Option<&mut Bus>;
}

#[derive(PartialEq, Eq, Clone, Debug)]
#[allow(clippy::upper_case_acronyms)]
pub enum BusType {
    PCI,
    #[allow(dead_code)]
    PCIE,
    #[allow(dead_code)]
    CCW,
    SCSI,
    #[allow(dead_code)]
    MMIO,
    SERIAL,
    NULL,
}

#[derive(Debug, Clone)]
pub struct Bus {
    pub(crate) r#type: BusType,
    pub(crate) id: String,
    pub(crate) bus_addr: String,
    pub(crate) slots: Vec<Slot>,
}

impl Default for Bus {
    fn default() -> Bus {
        Bus {
            r#type: BusType::PCI,
            id: Default::default(),
            bus_addr: Default::default(),
            slots: Default::default(),
        }
    }
}

impl Bus {
    pub fn empty_slot(&self) -> Option<usize> {
        for (index, s) in self.slots.iter().enumerate() {
            if index == 0 {
                // NOTE: some version of qemu, index should start with 1
                continue;
            }
            if let SlotStatus::Empty = s.status {
                return Some(index);
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn attach<T: Device>(&mut self, device: &T) -> Result<usize> {
        for (index, s) in self.slots.iter_mut().enumerate() {
            if let SlotStatus::Empty = s.status {
                s.status = SlotStatus::Occupied(device.id());
                return Ok(index);
            }
        }
        Err(Error::ResourceExhausted("bus is full".to_string()))
    }

    #[allow(dead_code)]
    pub fn device_slot(&self, id: &str) -> Option<usize> {
        for (index, s) in self.slots.iter().enumerate() {
            if index == 0 {
                // NOTE: some version of qemu, index should start with 1
                continue;
            }
            if let SlotStatus::Occupied(s) = &s.status {
                if s == id {
                    return Some(index);
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub(crate) status: SlotStatus,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            status: SlotStatus::Empty,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SlotStatus {
    Empty,
    Occupied(String),
}

#[derive(Debug, Clone)]
pub enum Transport {
    Pci,
    Ccw,
    #[allow(dead_code)]
    Mmio,
}

impl Transport {
    pub fn to_driver(&self, ty: &str) -> String {
        format!(
            "{}-{}",
            ty,
            match self {
                Transport::Pci => {
                    "pci"
                }
                Transport::Ccw => {
                    "ccw"
                }
                Transport::Mmio => {
                    "device"
                }
            }
        )
    }

    pub fn disable_modern(&self, dis: bool) -> Option<bool> {
        match self {
            Transport::Pci => Some(dis),
            Transport::Ccw => None,
            Transport::Mmio => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DeviceInfo {
    Block(BlockDeviceInfo),
    #[allow(dead_code)]
    Tap(TapDeviceInfo),
    #[allow(dead_code)]
    Physical(PhysicalDeviceInfo),
    #[allow(dead_code)]
    VhostUser(VhostUserDeviceInfo),
    Char(CharDeviceInfo),
}

#[derive(Debug, Clone)]
pub struct BlockDeviceInfo {
    pub id: String,
    pub path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct TapDeviceInfo {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub mac_address: String,
    pub fds: Vec<RawFd>,
}

#[derive(Debug, Clone)]
pub struct PhysicalDeviceInfo {
    pub id: String,
    pub bdf: String,
}

#[derive(Debug, Clone)]
pub struct VhostUserDeviceInfo {
    pub id: String,
    pub socket_path: String,
    pub mac_address: String,
    pub r#type: String,
}

#[derive(Debug, Clone)]
pub struct CharDeviceInfo {
    pub id: String,
    pub chardev_id: String,
    pub name: String,
    pub backend: CharBackendType,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CharBackendType {
    Pipe(String),
    #[allow(dead_code)]
    Socket(String),
}
