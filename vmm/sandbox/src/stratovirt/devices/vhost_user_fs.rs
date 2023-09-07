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

const VHOST_USER_FS_DRIVER: &str = "vhost-user-fs";
pub const DEFAULT_MOUNT_TAG_NAME: &str = "kuasar";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct VhostUserFs {
    #[property(ignore_key)]
    pub driver: String,
    pub id: String,
    #[property(key = "chardev")]
    pub chardev_id: String,
    pub tag: String,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub bus: String,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub addr: String,
}

impl_device_no_bus!(VhostUserFs);
impl_set_get_device_addr!(VhostUserFs);

impl VhostUserFs {
    pub fn new(id: &str, transport: Transport, chardev: &str, mount_tag: &str, bus: &str) -> Self {
        Self {
            driver: transport.to_driver(VHOST_USER_FS_DRIVER),
            id: id.to_string(),
            chardev_id: chardev.to_string(),
            tag: mount_tag.to_string(),
            bus: bus.to_string(),
            addr: "".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VhostUserFs;
    use crate::{
        device::Transport,
        param::ToCmdLineParams,
        stratovirt::devices::{
            char::CharDevice, device::GetAndSetDeviceAddr, DEFAULT_PCIE_BUS, VHOST_USER_FS_ADDR,
        },
    };

    #[test]
    fn test_vhost_user_fs_device_params() {
        let absolute_virtiofs_sock = "/path/to/virtiofs.sock";
        let chardev_id = "virtio-fs-test";
        let virtiofs_chardev = CharDevice::new("socket", chardev_id, absolute_virtiofs_sock);
        let virtiofs_chardev_params = virtiofs_chardev.to_cmdline_params("-");

        let mut result_params: Vec<String> = vec![];
        for s in virtiofs_chardev_params.into_iter() {
            result_params.push(s);
        }

        let mut vhost_user_fs_device = VhostUserFs::new(
            "vhost-user-fs-test",
            Transport::Pci,
            chardev_id,
            "myfs",
            DEFAULT_PCIE_BUS,
        );
        vhost_user_fs_device.set_device_addr(VHOST_USER_FS_ADDR);
        let vhost_user_fs_device_cmd_params = vhost_user_fs_device.to_cmdline_params("-");
        for s in vhost_user_fs_device_cmd_params.into_iter() {
            result_params.push(s);
        }

        println!("vhost-user-fs device params: {:?}", result_params);

        let expected_params: Vec<String> = vec![
            "-chardev",
            "socket,id=virtio-fs-test,path=/path/to/virtiofs.sock,server,nowait",
            "-device", 
            "vhost-user-fs-pci,id=vhost-user-fs-test,chardev=virtio-fs-test,tag=myfs,bus=pcie.0,addr=0x4",
            ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(expected_params, result_params);
    }
}
