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
use containerd_sandbox::error::Result;
use vmm_common::{CGROUP_NAMESPACE, IPC_NAMESPACE, NET_NAMESPACE, SANDBOX_NS_PATH, UTS_NAMESPACE};

use crate::{container::handler::Handler, sandbox::KuasarSandbox, vm::VM};

pub struct NamespaceHandler {
    container_id: String,
}

impl NamespaceHandler {
    pub fn new(container_id: &str) -> Self {
        Self {
            container_id: container_id.to_string(),
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for NamespaceHandler
where
    T: VM + Sync + Send,
{
    async fn handle(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let container = sandbox.container_mut(&self.container_id)?;
        let spec = if let Some(s) = &mut container.data.spec {
            s
        } else {
            return Ok(());
        };
        if let Some(l) = spec.linux.as_mut() {
            l.namespaces
                .retain(|n| n.r#type != NET_NAMESPACE && n.r#type != CGROUP_NAMESPACE);
            l.namespaces.iter_mut().for_each(|n| {
                n.path = if n.r#type == IPC_NAMESPACE || n.r#type == UTS_NAMESPACE {
                    format!("{}/{}", SANDBOX_NS_PATH, n.r#type)
                } else {
                    "".to_string()
                }
            });
        };
        Ok(())
    }

    async fn rollback(&self, _sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        Ok(())
    }
}
