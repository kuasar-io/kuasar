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

use crate::device::Transport;

const VIRTIO_SERIAL_DRIVER: &str = "virtio-serial";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct SerialDevice {
    #[property(ignore_key)]
    pub driver: String,
    pub id: String,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub bus: String,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub addr: String,
    #[property(ignore)]
    pub transport: Transport,
}

impl SerialDevice {
    pub fn new(id: &str, transport: Transport, bus: &str) -> Self {
        SerialDevice {
            driver: transport.to_driver(VIRTIO_SERIAL_DRIVER),
            id: id.to_string(),
            bus: bus.to_string(),
            addr: "".to_string(),
            transport,
        }
    }
}

impl_device_no_bus!(SerialDevice);
impl_set_get_device_addr!(SerialDevice);

#[cfg(test)]
mod tests {
    use super::SerialDevice;
    use crate::{
        device::Transport,
        param::ToCmdLineParams,
        stratovirt::devices::{
            device::GetAndSetDeviceAddr, DEFAULT_PCIE_BUS, VIRTIO_SERIAL_CONSOLE_ADDR,
        },
    };

    #[test]
    fn test_serial_device_params() {
        let mut serial_device =
            SerialDevice::new("virtio-serial0", Transport::Pci, DEFAULT_PCIE_BUS);
        serial_device.set_device_addr(VIRTIO_SERIAL_CONSOLE_ADDR);
        let serial_device_cmd_params = serial_device.to_cmdline_params("-");
        println!("root device params: {:?}", serial_device_cmd_params);

        let expected_params: Vec<String> = vec![
            "-device",
            "virtio-serial-pci,id=virtio-serial0,bus=pcie.0,addr=0x2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(expected_params, serial_device_cmd_params);
    }
}
