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
    #[property(param = "device")]
    pub(crate) romfile: Option<String>,
    #[property(param = "device")]
    pub(crate) max_bytes: Option<u32>,
    #[property(param = "device")]
    pub(crate) period: Option<u32>,
}

impl_device_no_bus!(VirtioRngDevice);

impl VirtioRngDevice {
    pub fn new(id: &str, filename: &str, transport: Transport) -> Self {
        Self {
            id: id.to_string(),
            object_type: "rng-random".to_string(),
            filename: filename.to_string(),
            driver: transport.to_driver(VIRTIO_RNG_DRIVER),
            romfile: None,
            max_bytes: None,
            period: None,
        }
    }
}
