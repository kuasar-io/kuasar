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
#[params("device", "fsdev")]
pub struct Virtio9PDevice {
    #[property(param = "device", ignore_key)]
    pub driver: String,
    #[property(param = "fsdev", ignore_key)]
    pub fs_driver: String,
    #[property(param = "device", key = "fsdev")]
    #[property(param = "fsdev")]
    pub id: String,
    #[property(param = "fsdev")]
    pub path: String,
    #[property(param = "device", key = "mount_tag")]
    pub mount_tag: String,
    #[property(param = "fsdev", key = "security_model")]
    pub security_model: String,
    #[property(param = "device")]
    pub disable_modern: Option<bool>,
    #[property(param = "device")]
    pub romfile: Option<String>,
    #[property(param = "fsdev")]
    pub multidevs: Option<String>,
    #[property(
        param = "device",
        key = "writeout",
        predicate = "self.direct_io==true",
        generator = "writeout"
    )]
    pub direct_io: bool,
}

impl_device_no_bus!(Virtio9PDevice);

impl Virtio9PDevice {
    pub fn new(
        id: &str,
        path: &str,
        mount_tag: &str,
        direct_io: bool,
        multidevs: Option<String>,
        transport: Transport,
    ) -> Self {
        Self {
            driver: transport.to_driver("virtio-9p"),
            fs_driver: "local".to_string(),
            id: id.to_string(),
            path: path.to_string(),
            mount_tag: mount_tag.to_string(),
            security_model: "none".to_string(),
            disable_modern: None,
            romfile: None,
            multidevs,
            direct_io,
        }
    }
}

fn writeout(direct: &bool) -> String {
    if *direct {
        "immediate".to_string()
    } else {
        "".to_string()
    }
}
