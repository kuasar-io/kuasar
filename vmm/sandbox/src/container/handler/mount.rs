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
use containerd_sandbox::{error::Result, spec::Mount};

use crate::{container::handler::Handler, vm::VM, KuasarSandbox};

pub struct MountHandler {
    mount: Mount,
    container_id: String,
}

impl MountHandler {
    pub fn new(container_id: &str, mount: Mount) -> Self {
        Self {
            mount,
            container_id: container_id.to_string(),
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for MountHandler
where
    T: VM + Sync + Send,
{
    async fn handle(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        sandbox
            .attach_storage(&self.container_id, &self.mount)
            .await
    }

    async fn rollback(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        sandbox
            .deference_storage(&self.container_id, &self.mount)
            .await
    }
}
