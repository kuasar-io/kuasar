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
use serde_derive::Serialize;

#[derive(CmdLineParams, Debug, Clone)]
pub struct Disk {
    path: String,
    id: String,
    readonly: Option<bool>,
    direct: Option<bool>,
    iommu: Option<bool>,
    num_queues: Option<u32>,
    bw_refill_time: Option<u32>,
    ops_size: Option<u32>,
    ope_one_time_burst: Option<u32>,
    ops_refill_time: Option<u32>,
    pci_segment: Option<String>,
}

impl_device_no_bus!(Disk);

impl Disk {
    pub fn new(id: &str, path: &str, readonly: bool, direct: bool) -> Self {
        Self {
            path: path.to_string(),
            id: id.to_string(),
            readonly: Some(readonly),
            direct: Some(direct),
            iommu: None,
            num_queues: None,
            bw_refill_time: None,
            ops_size: None,
            ope_one_time_burst: None,
            ops_refill_time: None,
            pci_segment: None,
        }
    }
}

#[derive(Serialize, Debug)]
pub struct DiskConfig {
    pub path: String,
    pub readonly: bool,
    pub direct: bool,
    pub vhost_user: bool,
    pub vhost_socket: Option<String>,
    pub id: String,
}
