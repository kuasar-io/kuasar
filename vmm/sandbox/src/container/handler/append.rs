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

use async_trait::async_trait;
use containerd_sandbox::{error::Result, ContainerOption};

use crate::{
    container::{handler::Handler, KuasarContainer},
    vm::VM,
    KuasarSandbox,
};

pub struct MetadataAddHandler {
    id: String,
    option: ContainerOption,
}

impl MetadataAddHandler {
    pub fn new(id: &str, option: ContainerOption) -> Self {
        Self {
            id: id.to_string(),
            option,
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for MetadataAddHandler
where
    T: VM + Sync + Send,
{
    async fn handle(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let container = KuasarContainer {
            id: self.id.to_string(),
            data: self.option.container.clone(),
            io_devices: vec![],
            processes: vec![],
        };
        sandbox.containers.insert(self.id.clone(), container);
        Ok(())
    }

    async fn rollback(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        sandbox.containers.remove(&*self.id);
        Ok(())
    }
}
