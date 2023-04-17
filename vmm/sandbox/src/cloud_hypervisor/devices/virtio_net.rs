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

use std::os::unix::io::RawFd;

use sandbox_derive::CmdLineParams;

#[derive(CmdLineParams, Debug, Clone)]
#[params("net")]
pub struct VirtioNetDevice {
    id: String,

    #[property(key = "tap", predicate = "self.fds.len()<=0")]
    pub(crate) ifname: Option<String>,

    #[property(
        key = "fd",
        predicate = "self.fds.len()>0",
        generator = "vec_to_string"
    )]
    pub(crate) fds: Vec<i32>,

    pub(crate) mac: String,

    #[property(key = "num_queues", predicate = "self.fds.len()>0")]
    pub(crate) num_queues: u32,
}

impl_device_no_bus!(VirtioNetDevice);

impl VirtioNetDevice {
    pub fn new(id: &str, name: Option<String>, mac: &str, fds: Vec<RawFd>) -> Self {
        Self {
            id: id.to_string(),
            ifname: name,
            num_queues: (fds.len() * 2) as u32,
            mac: mac.to_string(),
            fds,
        }
    }
}

pub fn vec_to_string<T: ToString>(v: &[T]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<String>>()
            .join(",")
    )
}
