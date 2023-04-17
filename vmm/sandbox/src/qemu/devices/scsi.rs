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

use sandbox_derive::CmdLineParams;

use crate::device::{Bus, BusType, Slot, Transport};

pub const VIRTIO_SCSI_DRIVER: &str = "virtio-scsi";
// actually the max capacity of scsi bus is 65535, but we don't want to create that many slots
pub(crate) const SCSI_BUS_CAPACITY: usize = 64;

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct ScsiController {
    #[property(ignore_key)]
    pub(crate) driver: String,
    pub(crate) id: String,
    #[property(key = "bus")]
    pub(crate) bus_addr: Option<String>,
    pub(crate) addr: Option<String>,
    pub(crate) disable_modern: Option<bool>,
    pub(crate) iothread: Option<String>,
    pub(crate) romfile: Option<String>,
    #[property(ignore)]
    pub(crate) bus: Bus,
}

impl_device!(ScsiController);

impl ScsiController {
    pub fn new(id: &str, transport: Transport) -> ScsiController {
        ScsiController {
            driver: transport.to_driver(VIRTIO_SCSI_DRIVER),
            id: id.to_string(),
            bus_addr: None,
            addr: None,
            disable_modern: transport.disable_modern(false),
            iothread: None,
            romfile: None,
            bus: Bus {
                r#type: BusType::SCSI,
                id: id.to_string(),
                bus_addr: "0:0".to_string(),
                slots: vec![Slot::default(); SCSI_BUS_CAPACITY],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        device::{Bus, BusType, Transport},
        param::ToParams,
        qemu::devices::scsi::{ScsiController, VIRTIO_SCSI_DRIVER},
    };

    #[test]
    fn test_scsi_controller_to_params() {
        let c = ScsiController {
            driver: Transport::Pci.to_driver(VIRTIO_SCSI_DRIVER),
            id: "fakeid".to_string(),
            bus_addr: Some("scsi0".to_string()),
            addr: Some("1".to_string()),
            disable_modern: Some(false),
            iothread: None,
            romfile: None,
            bus: Bus {
                r#type: BusType::PCI,
                id: "".to_string(),
                bus_addr: "".to_string(),
                slots: vec![],
            },
        };
        let params = c.to_params();
        let param = params.get(0).unwrap();
        assert!(param.get("iothread").is_none());
        assert_eq!(param.get("bus").unwrap(), "scsi0");
        assert_eq!(param.get("addr").unwrap(), "1");
        assert!(param.get("romfile").is_none());
        assert_eq!(param.get("id").unwrap(), "fakeid");
    }
}
