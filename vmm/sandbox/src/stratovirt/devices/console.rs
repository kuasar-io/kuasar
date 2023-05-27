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

pub const VIRT_CONSOLE_DRIVER: &str = "virtconsole";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct VirtConsole {
    #[property(ignore_key)]
    pub driver: String,
    pub id: String,
    pub chardev: String,
    #[property(ignore)]
    pub addr: String,
}

impl_device_no_bus!(VirtConsole);
impl_set_get_device_addr!(VirtConsole);

impl VirtConsole {
    pub fn new(id: &str, chardev_id: &str) -> Self {
        Self {
            driver: VIRT_CONSOLE_DRIVER.to_string(),
            id: id.to_string(),
            chardev: chardev_id.to_string(),
            addr: "".to_string(),
        }
    }
}

mod tests {
    #[allow(unused_imports)]
    use super::VirtConsole;
    #[allow(unused_imports)]
    use crate::{
        device::Transport,
        param::ToCmdLineParams,
        stratovirt::devices::{
            char::CharDevice, device::GetAndSetDeviceAddr, serial::SerialDevice, DEFAULT_PCIE_BUS,
            VIRTIO_SERIAL_CONSOLE_ADDR,
        },
    };

    #[test]
    fn test_virtconsole_device_params() {
        let mut result_params: Vec<String> = vec![];

        let mut virtio_serial_device =
            SerialDevice::new("virtio-serial0", Transport::Pci, DEFAULT_PCIE_BUS);
        virtio_serial_device.set_device_addr(VIRTIO_SERIAL_CONSOLE_ADDR);
        let virtio_serial_device_params = virtio_serial_device.to_cmdline_params("-");
        for s in virtio_serial_device_params.into_iter() {
            result_params.push(s);
        }

        let console_backend_chardev = CharDevice::new(
            "socket",
            "charconsole0",
            "/run/vc/vm/sandbox-id/console.sock",
        );

        let char_device_cmd_params = console_backend_chardev.to_cmdline_params("-");
        for s in char_device_cmd_params.into_iter() {
            result_params.push(s);
        }

        let virtconsole_device = VirtConsole::new("virtio-console0", "charconsole0");

        let virtconsole_device_cmd_params = virtconsole_device.to_cmdline_params("-");
        for s in virtconsole_device_cmd_params.into_iter() {
            result_params.push(s);
        }

        println!("virtconsole device params: {:?}", result_params);

        let expected_params: Vec<String> = vec![
            "-device",
            "virtio-serial-pci,id=virtio-serial0,bus=pcie.0,addr=0x2",
            "-chardev",
            "socket,id=charconsole0,path=/run/vc/vm/sandbox-id/console.sock,server,nowait",
            "-device",
            "virtconsole,id=virtio-console0,chardev=charconsole0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(expected_params, result_params);
    }
}
