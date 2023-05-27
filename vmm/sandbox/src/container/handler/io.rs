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
use containerd_sandbox::{data::Io, error::Result, Sandbox};
use log::debug;
use vmm_common::IO_FILE_PREFIX;

use crate::{
    container::handler::Handler, sandbox::KuasarSandbox, utils::write_file_atomic, vm::VM,
};

pub struct IoHandler {
    container_id: String,
    io: Io,
}

impl IoHandler {
    pub fn new(container_id: &str, io: Io) -> Self {
        Self {
            container_id: container_id.to_string(),
            io,
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for IoHandler
where
    T: VM + Sync + Send,
{
    async fn handle(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let mut io_devices = vec![];
        debug!("handle io {:?}", self.io);
        // TODO: what if it is not named pipe
        let stdin = attach_pipe(&self.io.stdin, sandbox, &mut io_devices).await?;
        let stdout = attach_pipe(&self.io.stdout, sandbox, &mut io_devices).await?;
        let stderr = attach_pipe(&self.io.stderr, sandbox, &mut io_devices).await?;
        let container = sandbox.container_mut(&self.container_id)?;
        container.data.io = Some(Io {
            stdin,
            stdout,
            stderr,
            terminal: self.io.terminal,
        });
        let io_str = serde_json::to_string(&container.data.io)
            .map_err(|e| anyhow!("failed to parse io in container, {}", e))?;
        let io_file_path = format!(
            "{}/{}-{}",
            container.data.bundle, IO_FILE_PREFIX, self.container_id
        );
        write_file_atomic(io_file_path, &io_str).await?;
        container.io_devices = io_devices;
        Ok(())
    }

    async fn rollback(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let container = sandbox.container(&self.container_id).await?;
        let bundle = container.data.bundle.to_string();
        for device_id in container.io_devices.clone() {
            sandbox.vm.hot_detach(&device_id).await?;
        }
        let io_file_path = format!("{}/{}-{}", bundle, IO_FILE_PREFIX, self.container_id);
        tokio::fs::remove_file(&io_file_path)
            .await
            .unwrap_or_default();
        let container = sandbox.container_mut(&self.container_id)?;
        container.io_devices = vec![];
        container.data.io = None;
        Ok(())
    }
}

pub async fn attach_pipe<T: VM + Sync + Send>(
    path: &str,
    sandbox: &mut KuasarSandbox<T>,
    io_devices: &mut Vec<String>,
) -> Result<String> {
    let name = if !path.is_empty() && !path.contains("vsock") {
        let (id, chardev_id) = sandbox.hot_attach_pipe(path).await?;
        io_devices.push(id);
        chardev_id
    } else {
        path.to_string()
    };
    Ok(name)
}
