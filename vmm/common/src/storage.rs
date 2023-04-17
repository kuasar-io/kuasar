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

use std::collections::HashMap;

use containerd_sandbox::spec::Mount;
use serde::{Deserialize, Serialize};

pub const ANNOTATION_KEY_STORAGE: &str = "io.kuasar.storages";

pub const DRIVER9PTYPE: &str = "9p";
pub const DRIVERVIRTIOFSTYPE: &str = "virtio-fs";
pub const DRIVERBLKTYPE: &str = "blk";
pub const DRIVERMMIOBLKTYPE: &str = "mmioblk";
pub const DRIVERSCSITYPE: &str = "scsi";
pub const DRIVERNVDIMMTYPE: &str = "nvdimm";
pub const DRIVEREPHEMERALTYPE: &str = "ephemeral";
pub const DRIVERLOCALTYPE: &str = "local";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Storage {
    pub host_source: String,
    pub r#type: String,
    pub id: String,
    pub device_id: Option<String>,
    pub ref_container: HashMap<String, u32>,
    pub need_guest_handle: bool,
    pub source: String,
    pub driver: String,
    pub driver_options: Vec<String>,
    pub fstype: String,
    pub options: Vec<String>,
    pub mount_point: String,
}

impl Storage {
    pub fn is_for_mount(&self, m: &Mount) -> bool {
        self.host_source == m.source && self.r#type == m.r#type
    }

    pub fn ref_count(&self) -> u32 {
        self.ref_container.iter().fold(0, |acc, (_k, v)| acc + v)
    }

    pub fn refer(&mut self, container_id: &str) {
        match self.ref_container.get_mut(container_id) {
            None => {
                self.ref_container.insert(container_id.to_string(), 1);
            }
            Some(r) => *r += 1,
        }
    }

    pub fn defer(&mut self, container_id: &str) {
        self.ref_container.remove(container_id);
    }
}
