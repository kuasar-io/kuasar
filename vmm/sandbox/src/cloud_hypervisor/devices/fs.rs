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
pub struct Fs {
    tag: String,
    socket: String,
    id: String,
}

impl_device_no_bus!(Fs);

impl Fs {
    pub fn new(id: &str, socket: &str, tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            socket: socket.to_string(),
            id: id.to_string(),
        }
    }
}
