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

const VFIO_DEVICE_SYSFS_PATH: &str = "/sys/bus/pci/devices";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct VfioDevice {
    #[property(ignore)]
    pub id: String,
    pub(crate) path: String,
}

impl_device_no_bus!(VfioDevice);

impl VfioDevice {
    pub fn new(id: &str, bdf: &str) -> Self {
        Self {
            id: id.to_string(),
            path: format!("{}/{}", VFIO_DEVICE_SYSFS_PATH, bdf),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{cloud_hypervisor::devices::vfio::VfioDevice, param::ToParams};

    #[test]
    fn test_attr() {
        let device = VfioDevice::new("", "0000:b4:05.1");
        let params = device.to_params();
        let property = params.get(0).unwrap();
        assert_eq!(
            property.get("path").unwrap(),
            "/sys/bus/pci/devices/0000:b4:05.1"
        );
    }
}
