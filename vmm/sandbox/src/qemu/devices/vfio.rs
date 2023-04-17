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

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct VfioDevice {
    #[property(ignore)]
    pub id: String,
    #[property(ignore_key)]
    pub(crate) driver: String,
    #[property(key = "host")]
    pub(crate) bdf: String,
    pub(crate) romfile: Option<String>,
}

impl_device_no_bus!(VfioDevice);

impl VfioDevice {
    pub fn new(id: &str, bdf: &str) -> Self {
        Self {
            id: id.to_string(),
            driver: Transport::Pci.to_driver("vfio"),
            bdf: bdf.to_string(),
            romfile: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{param::ToParams, qemu::devices::vfio::VfioDevice};

    #[test]
    fn test_attr() {
        let device = VfioDevice {
            id: "".to_string(),
            driver: "vfio-pci".to_string(),
            bdf: "0000:b4:05.1".to_string(),
            romfile: None,
        };
        let params = device.to_params();
        let property = params.get(0).unwrap();
        assert_eq!(property.get("driver").unwrap(), "vfio-pci");
        assert_eq!(property.get("host").unwrap(), "0000:b4:05.1");
    }
}
