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

use containerd_sandbox::error::Result;

use crate::{
    device::{CharBackendType, CharDeviceInfo, DeviceInfo},
    sandbox::KuasarSandbox,
    vm::VM,
};

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    pub async fn hot_attach_pipe(&mut self, path: &str) -> Result<(String, String)> {
        let id = self.increment_and_get_id();
        let device_id = format!("virtioserial{}", id);
        let chardev_id = format!("chardev{}", id);
        let char_dev = CharDeviceInfo {
            id: device_id.to_string(),
            chardev_id: chardev_id.to_string(),
            name: chardev_id.to_string(),
            backend: CharBackendType::Pipe(path.to_string()),
        };
        self.vm.hot_attach(DeviceInfo::Char(char_dev)).await?;
        Ok((device_id, chardev_id))
    }
}
