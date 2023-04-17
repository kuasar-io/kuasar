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

use crate::{container::handler::Handler, sandbox::KuasarSandbox, vm::VM};

#[allow(dead_code)]
pub const NAMESPACE_PID: &str = "pid";
pub const NAMESPACE_NET: &str = "network";
#[allow(dead_code)]
pub const NAMESPACE_MNT: &str = "mount";
pub const NAMESPACE_CGROUP: &str = "cgroup";

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
                .retain(|n| n.r#type != NAMESPACE_NET && n.r#type != NAMESPACE_CGROUP);
            l.namespaces
                .iter_mut()
                .for_each(|n| n.path = "".to_string());
        };
        Ok(())
    }

    async fn rollback(&self, _sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        Ok(())
    }
}
