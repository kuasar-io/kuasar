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
use containerd_sandbox::{
    data::{Io, ProcessData},
    Sandbox,
};
use vmm_common::IO_FILE_PREFIX;

use crate::{
    container::{
        handler::{io::attach_pipe, Handler},
        KuasarProcess,
    },
    sandbox::KuasarSandbox,
    utils::write_file_atomic,
    vm::VM,
};

pub struct ProcessHandler {
    container_id: String,
    proc: ProcessData,
}

impl ProcessHandler {
    pub fn new(container_id: &str, proc: ProcessData) -> Self {
        Self {
            container_id: container_id.to_string(),
            proc,
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for ProcessHandler
where
    T: VM + Sync + Send,
{
    async fn handle(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        let container = sandbox.container(&self.container_id).await?;
        let bundle = container.data.bundle.to_string();
        for proc in &container.processes {
            if proc.id == self.proc.id {
                return Ok(());
            }
        }
        let mut new_proc = KuasarProcess::new(self.proc.clone());
        if let Some(io) = &self.proc.io {
            let mut io_devices = vec![];
            // TODO what if it is not named pipe
            let stdin = attach_pipe(&io.stdin, sandbox, &mut io_devices).await?;
            let stdout = attach_pipe(&io.stdout, sandbox, &mut io_devices).await?;
            let stderr = attach_pipe(&io.stderr, sandbox, &mut io_devices).await?;
            new_proc.data.io = Some(Io {
                stdin,
                stdout,
                stderr,
                terminal: io.terminal,
            });
            new_proc.io_devices = io_devices;
            let io_str = serde_json::to_string(&new_proc.data.io)
                .map_err(|e| anyhow!("failed to parse io in container, {}", e))?;
            let io_file_path = format!(
                "{}/{}-{}-{}",
                bundle, IO_FILE_PREFIX, self.container_id, self.proc.id
            );
            write_file_atomic(io_file_path, &io_str).await?;
        }
        let container = sandbox.container_mut(&self.container_id)?;
        container.processes.push(new_proc);
        Ok(())
    }

    async fn rollback(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        remove_io_devices_for_process(sandbox, &self.container_id, &self.proc.id).await?;
        if let Ok(c) = sandbox.container_mut(&self.container_id) {
            c.processes.retain(|e| e.id != self.proc.id);
        }
        Ok(())
    }
}

pub struct ProcessRemoveHandler {
    container_id: String,
    process_id: String,
}

impl ProcessRemoveHandler {
    pub fn new(container_id: &str, process_id: &str) -> Self {
        Self {
            container_id: container_id.to_string(),
            process_id: process_id.to_string(),
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for ProcessRemoveHandler
where
    T: VM + Sync + Send,
{
    async fn handle(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        remove_io_devices_for_process(sandbox, &self.container_id, &self.process_id).await?;
        if let Ok(c) = sandbox.container_mut(&self.container_id) {
            c.processes.retain(|e| e.id != self.process_id);
        }

        Ok(())
    }

    async fn rollback(
        &self,
        _sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        Ok(())
    }
}

async fn remove_io_devices_for_process<T: VM + Sync + Send>(
    sandbox: &mut KuasarSandbox<T>,
    container_id: &str,
    process_id: &str,
) -> containerd_sandbox::error::Result<()> {
    let container = match sandbox.container(container_id).await {
        Ok(c) => c,
        Err(_e) => {
            return Ok(());
        }
    };
    let bundle = container.data.bundle.to_string();
    let mut io_devices = vec![];
    for proc in &container.processes {
        if proc.id == process_id {
            for device_id in &proc.io_devices {
                io_devices.push(device_id.to_string());
            }
        }
    }
    for device_id in io_devices {
        sandbox.vm.hot_detach(&device_id).await.unwrap_or_default();
    }
    let io_file_path = format!(
        "{}/{}-{}-{}",
        bundle, IO_FILE_PREFIX, container_id, process_id
    );
    tokio::fs::remove_file(&io_file_path)
        .await
        .unwrap_or_default();
    Ok(())
}
