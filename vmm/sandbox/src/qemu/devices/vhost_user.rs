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

use std::fmt::{Display, Formatter};

use sandbox_derive::CmdLineParams;

#[derive(Debug, Clone)]
pub enum VhostUserType {
    VhostUserNet(String),
    VhostUserChar(String),
}

impl Display for VhostUserType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match &self {
            VhostUserType::VhostUserNet(r#type) => r#type.to_string(),
            VhostUserType::VhostUserChar(r#type) => r#type.to_string(),
        };
        write!(f, "{}", s)
    }
}

#[derive(CmdLineParams, Debug, Clone)]
#[params("device", "netdev")]
pub struct VhostNetDevice {
    #[property(param = "device", ignore_key)]
    pub(crate) driver: VhostUserType,
    #[property(ignore)]
    pub id: String,
    #[property(param = "netdev", key = "type")]
    pub(crate) netdev_type: String,
    #[property(param = "netdev", ignore_key)]
    pub(crate) netdev_param: String,
    #[property(param = "netdev", key = "chardev")]
    pub(crate) char_dev_id: String,
    #[property(param = "device", key = "netdev")]
    #[property(param = "netdev", key = "id")]
    pub(crate) net_dev_id: String,
    #[property(param = "device", key = "mac")]
    pub(crate) address: Option<String>,
    #[property(param = "device")]
    pub(crate) tag: Option<String>,
    #[property(param = "device")]
    pub(crate) cache_size: Option<u32>,
    #[property(param = "device")]
    pub(crate) shared_versions: Option<bool>,
    #[property(param = "device")]
    pub(crate) romfile: Option<String>,
}

impl_device_no_bus!(VhostNetDevice);

impl VhostNetDevice {
    pub fn new(id: &str, t: VhostUserType, _socket_path: &str, address: &str) -> Self {
        // TODO: length must <= 31
        let char_dev_id: String = format!("{}-{}", "char", id);
        let type_dev_id = format!("{}-{}", "net", id);
        Self {
            id: id.to_string(),
            driver: t,
            netdev_type: "vhost-user".to_string(),
            netdev_param: "vhostforce".to_string(),
            char_dev_id,
            net_dev_id: type_dev_id,
            address: Some(address.to_string()),
            tag: None,
            cache_size: None,
            shared_versions: None,
            romfile: Some("".to_string()),
        }
    }
}

#[derive(CmdLineParams, Debug, Clone)]
#[params("device", "chardev")]
pub struct VhostCharDevice {
    #[property(param = "device", ignore_key)]
    pub(crate) driver: VhostUserType,
    #[property(param = "chardev", ignore_key)]
    pub(crate) chardev_type: String,
    #[property(ignore)]
    pub id: String,
    #[property(param = "chardev", key = "path")]
    pub(crate) socket_path: String,
    #[property(param = "chardev", key = "id")]
    pub(crate) char_dev_id: String,
    #[property(param = "device", key = "chardev")]
    pub(crate) net_dev_id: String,
    #[property(param = "device")]
    pub(crate) tag: Option<String>,
    #[property(param = "device")]
    pub(crate) cache_size: Option<u32>,
    #[property(param = "device")]
    pub(crate) shared_versions: Option<bool>,
    #[property(param = "device")]
    pub(crate) romfile: Option<String>,
    #[cfg(feature = "virtcca")]
    #[property(
        param = "device",
        key = "iommu_platform",
        generator = "crate::utils::bool_to_on_off"
    )]
    pub iommu_platform: bool,
}

impl_device_no_bus!(VhostCharDevice);

impl VhostCharDevice {
    pub fn new(id: &str, t: VhostUserType, socket_path: &str, _address: &str) -> Self {
        // TODO: length must <= 31
        let char_dev_id: String = format!("{}-{}", "char", id);
        let type_dev_id = format!("{}-{}", "char", id);
        Self {
            id: id.to_string(),
            driver: t,
            chardev_type: "socket".to_string(),
            socket_path: socket_path.to_string(),
            char_dev_id,
            net_dev_id: type_dev_id,
            tag: Some("kuasar".to_string()),
            cache_size: None,
            shared_versions: None,
            romfile: Some("".to_string()),
            #[cfg(feature = "virtcca")]
            iommu_platform: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        param::ToParams,
        qemu::devices::vhost_user::{VhostNetDevice, VhostUserType},
    };

    #[test]
    fn test_attr() {
        let device = VhostNetDevice::new(
            "123",
            VhostUserType::VhostUserNet("virtio-net-pci".to_string()),
            "/var/run/vhost-user/tap07dc8d7f-fd",
            "fa:16:3e:ce:ac:af",
        );
        let params = device.to_params();
        for param in params {
            if param.name == "device" {
                assert_eq!(param.get("driver").unwrap(), "virtio-net-pci");
                assert_eq!(param.get("mac").unwrap(), "fa:16:3e:ce:ac:af");
            }
        }
    }
}
