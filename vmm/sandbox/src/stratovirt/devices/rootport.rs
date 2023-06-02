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

use crate::device::Bus;

pub(crate) const PCIE_ROOT_PORT_DRIVER: &str = "pcie-root-port";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct RootPort {
    #[property(ignore_key)]
    pub(crate) device_type: String,
    pub(crate) id: String,
    pub(crate) port: String,
    #[property(key = "bus")]
    pub(crate) bus_name: String,
    pub(crate) addr: String,
    #[property(key = "multifunction")]
    pub(crate) multi_function: Option<bool>,
    #[property(ignore)]
    pub(crate) device_id: String,
    #[property(ignore)]
    pub(crate) index: usize,
}

impl RootPort {
    pub fn new(id: &str, port: usize, index: usize, bus: &str, multi_func: Option<bool>) -> Self {
        Self {
            device_type: PCIE_ROOT_PORT_DRIVER.to_string(),
            id: id.to_string(),
            port: format!("{:#02x}", port),
            bus_name: bus.to_string(),
            addr: "".to_string(),
            multi_function: multi_func,
            device_id: "".to_string(),
            index,
        }
    }
}

impl_device_no_bus!(RootPort);
impl_set_get_device_addr!(RootPort);

#[derive(Debug, Clone, Default)]
pub struct PCIERootPorts {
    pub(crate) id: String,
    pub(crate) bus: Bus,
    pub(crate) root_ports: Vec<RootPort>,
}

impl_device!(PCIERootPorts);

#[cfg(test)]
mod tests {
    use super::RootPort;
    use crate::{
        param::ToCmdLineParams,
        stratovirt::devices::{
            device::GetAndSetDeviceAddr, DEFAULT_PCIE_BUS, ROOTPORT_PCI_START_ADDR,
        },
    };

    #[test]
    fn test_root_port_params() {
        let mut root_port = RootPort::new(
            "pcie.1",
            ROOTPORT_PCI_START_ADDR,
            ROOTPORT_PCI_START_ADDR,
            DEFAULT_PCIE_BUS,
            Some(false),
        );
        root_port.set_device_addr(ROOTPORT_PCI_START_ADDR);
        let root_port_cmd_params = root_port.to_cmdline_params("-");
        println!("root device params: {:?}", root_port_cmd_params);

        let expected_params: Vec<String> = vec![
            "-device",
            "pcie-root-port,id=pcie.1,port=0x5,bus=pcie.0,addr=0x5,multifunction=false",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(expected_params, root_port_cmd_params);
    }
}
