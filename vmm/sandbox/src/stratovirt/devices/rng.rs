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

pub const VIRTIO_RNG_DRIVER: &str = "virtio-rng";

#[derive(CmdLineParams, Debug, Clone)]
#[params("object", "device")]
pub struct VirtioRngDevice {
    #[property(param = "object", ignore_key)]
    pub(crate) object_type: String,
    #[property(param = "device", ignore_key)]
    pub(crate) driver: String,
    #[property(param = "object")]
    #[property(param = "device", key = "rng")]
    pub(crate) id: String,
    #[property(param = "object")]
    pub(crate) filename: String,
    #[property(param = "device", key = "id")]
    pub(crate) device_id: String,
    #[property(param = "device")]
    pub(crate) max_bytes: Option<u32>,
    #[property(param = "device")]
    pub(crate) period: Option<u32>,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub(crate) bus: String,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub(crate) addr: String,
}

impl_device_no_bus!(VirtioRngDevice);
impl_set_get_device_addr!(VirtioRngDevice);

impl VirtioRngDevice {
    pub fn new(id: &str, filename: &str, transport: Transport, bus: &str) -> Self {
        Self {
            object_type: "rng-random".to_string(),
            driver: transport.to_driver(VIRTIO_RNG_DRIVER),
            id: id.to_string(),
            filename: filename.to_string(),
            device_id: format!("virtio-{}", id),
            max_bytes: None,
            period: None,
            bus: bus.to_string(),
            addr: "".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VirtioRngDevice;
    use crate::{
        device::Transport,
        param::ToCmdLineParams,
        stratovirt::devices::{
            device::GetAndSetDeviceAddr, DEFAULT_PCIE_BUS, VIRTIO_RND_DEVICE_ADDR,
        },
    };

    #[test]
    fn test_virtio_rng_params() {
        let mut virtio_rng_device =
            VirtioRngDevice::new("rng0", "/dev/urandom", Transport::Pci, DEFAULT_PCIE_BUS);
        virtio_rng_device.set_device_addr(VIRTIO_RND_DEVICE_ADDR);
        let virtio_rng_device_cmd_params = virtio_rng_device.to_cmdline_params("-");
        println!(
            "virtio-rng device params: {:?}",
            virtio_rng_device_cmd_params
        );

        let expected_params: Vec<String> = vec![
            "-object",
            "rng-random,id=rng0,filename=/dev/urandom",
            "-device",
            "virtio-rng-pci,rng=rng0,id=virtio-rng0,bus=pcie.0,addr=0x1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(true);
    }
}
