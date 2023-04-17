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

#[derive(CmdLineParams, Debug, Clone)]
pub struct Pmem {
    id: String,
    file: String,
    size: Option<u64>,
    #[property(generator = "crate::utils::bool_to_on_off")]
    iommu: Option<bool>,
    #[property(key = "discard_writes", generator = "crate::utils::bool_to_on_off")]
    discard_writes: Option<bool>,
}

impl_device_no_bus!(Pmem);

impl Pmem {
    pub fn new(id: &str, file: &str, discard_writes: bool) -> Self {
        Self {
            id: id.to_string(),
            file: file.to_string(),
            size: None,
            iommu: None,
            discard_writes: Some(discard_writes),
        }
    }
}
