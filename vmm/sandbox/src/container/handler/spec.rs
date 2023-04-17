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

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::Error;

use crate::{
    container::handler::Handler, sandbox::KuasarSandbox, utils::write_file_atomic, vm::VM,
};

const CONFIG_FILE_NAME: &str = "config.json";

pub struct SpecHandler {
    container_id: String,
}

impl SpecHandler {
    pub fn new(container_id: &str) -> Self {
        Self {
            container_id: container_id.to_string(),
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for SpecHandler
where
    T: VM + Sync + Send,
{
    async fn handle(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        let container = sandbox.container_mut(&self.container_id)?;
        let spec = container
            .data
            .spec
            .as_mut()
            .ok_or(Error::InvalidArgument(format!(
                "no spec for container {}",
                self.container_id
            )))?;
        // TODO support apparmor in guest os, but not remove the apparmor profile here
        if let Some(p) = spec.process.as_mut() {
            p.apparmor_profile = "".to_string();
        }
        let spec_str = serde_json::to_string(spec)
            .map_err(|e| anyhow!("failed to parse spec in sandbox, {}", e))?;
        let bundle = format!("{}/{}", sandbox.base_dir, self.container_id);
        tokio::fs::create_dir_all(&*bundle)
            .await
            .map_err(|e| anyhow!("failed to create container bundle, {}", e))?;
        let config_path = format!("{}/{}", bundle, CONFIG_FILE_NAME);
        write_file_atomic(config_path, &spec_str).await?;
        let container = sandbox.container_mut(&self.container_id)?;
        container.data.bundle = bundle;
        Ok(())
    }

    async fn rollback(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        let bundle = format!("{}/{}", sandbox.base_dir, self.container_id);
        tokio::fs::remove_dir_all(&*bundle)
            .await
            .map_err(|e| anyhow!("failed to remove container bundle, {}", e))?;
        Ok(())
    }
}
