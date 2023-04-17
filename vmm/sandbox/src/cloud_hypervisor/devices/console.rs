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
pub struct Console {
    #[property(ignore)]
    id: String,
    file: Option<String>,
    iommu: Option<bool>,
}

impl_device_no_bus!(Console);

impl Console {
    pub fn new(path: &str, id: &str) -> Self {
        Self {
            id: id.to_string(),
            file: Some(path.to_string()),
            iommu: None,
        }
    }
}
