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

use std::{collections::HashMap, sync::Arc};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    data::{ContainerData, SandboxData},
    error::{Error, Result},
    signal::ExitSignal,
    Container, ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use containerd_shim::{
    asynchronous::task::TaskService,
    protos::{shim::shim_ttrpc_async::create_task, ttrpc::asynchronous::Server},
};
use log::debug;
use tokio::{
    fs::create_dir_all,
    sync::{mpsc::channel, Mutex, RwLock},
};

#[cfg(feature = "wasmedge")]
use crate::wasmedge::{process_exits, WasmEdgeContainer, WasmEdgeContainerFactory};
#[cfg(feature = "wasmtime")]
use crate::wasmtime::{exec_exits, WasmtimeContainer, WasmtimeContainerFactory};

#[derive(Default)]
pub struct WasmSandboxer {
    #[allow(clippy::type_complexity)]
    pub(crate) sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<WasmSandbox>>>>>,
}

pub struct WasmSandbox {
    pub(crate) _id: String,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) status: SandboxStatus,
    pub(crate) exit_signal: Arc<ExitSignal>,
    pub(crate) containers: HashMap<String, WasmContainer>,
    pub(crate) server: Option<Server>,
}

pub struct WasmContainer {
    pub(crate) data: ContainerData,
}

#[async_trait]
impl Sandboxer for WasmSandboxer {
    type Sandbox = WasmSandbox;

    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        let sandbox = WasmSandbox {
            _id: id.to_string(),
            base_dir: s.base_dir,
            data: s.sandbox,
            status: SandboxStatus::Created,
            exit_signal: Arc::new(Default::default()),
            containers: Default::default(),
            server: None,
        };
        create_dir_all(&sandbox.base_dir)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", sandbox.base_dir, e))?;
        let mut sandboxes = self.sandboxes.write().await;
        sandboxes.insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    async fn start(&self, id: &str) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        sandbox.lock().await.start().await?;
        Ok(())
    }

    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        sandbox.lock().await.data = data;
        Ok(())
    }

    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone())
    }

    async fn stop(&self, id: &str, _force: bool) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        sandbox.lock().await.stop().await?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        if let Some(sandbox) = self.sandboxes.write().await.remove(id) {
            let mut sandbox = sandbox.lock().await;
            if let Some(mut server) = sandbox.server.take() {
                server
                    .shutdown()
                    .await
                    .map_err(|e| anyhow!("failed to shutdown task server, {}", e))?;
            }
        }
        Ok(())
    }
}

impl WasmSandbox {
    async fn stop(&mut self) -> Result<()> {
        if let Some(mut server) = self.server.take() {
            server
                .shutdown()
                .await
                .map_err(|e| anyhow!("failed to shutdown task server, {}", e))?;
        }
        let ts = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        self.status = SandboxStatus::Stopped(0, ts);
        self.exit_signal.signal();
        Ok(())
    }

    async fn start(&mut self) -> Result<()> {
        let task = self.start_task_service().await?;
        let task_address = format!("unix://{}/task.sock", self.base_dir);
        self.data
            .task_address
            .clone_from(&format!("ttrpc+{}", task_address));
        let task_service = create_task(Arc::new(Box::new(task)));
        let mut server = Server::new().register_service(task_service);
        server = server
            .bind(&task_address)
            .map_err(|e| anyhow!("failed to bind socket {}, {}", task_address, e))?;
        server
            .start()
            .await
            .map_err(|e| anyhow!("failed to start task server, {}", e))?;
        self.server = Some(server);
        self.status = SandboxStatus::Running(0);
        Ok(())
    }

    #[cfg(feature = "wasmedge")]
    async fn start_task_service(
        &self,
    ) -> Result<TaskService<WasmEdgeContainerFactory, WasmEdgeContainer>> {
        let (tx, mut rx) = channel(128);
        let mut factory = WasmEdgeContainerFactory::default();
        factory.netns.clone_from(&self.data.netns);
        let task = TaskService {
            factory,
            containers: Arc::new(Default::default()),
            namespace: "k8s.io".to_string(),
            exit: Arc::new(Default::default()),
            tx: tx.clone(),
        };

        process_exits(&task).await;

        tokio::spawn(async move {
            while let Some((_topic, e)) = rx.recv().await {
                debug!("received event {:?}", e);
            }
        });
        Ok(task)
    }

    #[cfg(feature = "wasmtime")]
    async fn start_task_service(
        &self,
    ) -> Result<TaskService<WasmtimeContainerFactory, WasmtimeContainer>> {
        let (tx, mut rx) = channel(128);
        let factory = WasmtimeContainerFactory {
            netns: self.data.netns.clone(),
        };
        let task = TaskService {
            factory,
            containers: Arc::new(Default::default()),
            namespace: "k8s.io".to_string(),
            exit: Arc::new(Default::default()),
            tx: tx.clone(),
        };
        exec_exits(&task).await;
        tokio::spawn(async move {
            while let Some((_topic, e)) = rx.recv().await {
                debug!("received event {:?}", e);
            }
        });
        Ok(task)
    }
}

#[async_trait]
impl Sandbox for WasmSandbox {
    type Container = WasmContainer;

    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn container(&self, id: &str) -> Result<&Self::Container> {
        self.containers.get(id).ok_or(Error::NotFound(format!(
            "failed to find container by id {id}"
        )))
    }

    async fn append_container(&mut self, id: &str, option: ContainerOption) -> Result<()> {
        let container = WasmContainer {
            data: option.container,
        };
        self.containers.insert(id.to_string(), container);
        Ok(())
    }

    async fn update_container(&mut self, _id: &str, _option: ContainerOption) -> Result<()> {
        Ok(())
    }

    async fn remove_container(&mut self, id: &str) -> Result<()> {
        self.containers.remove(id);
        Ok(())
    }

    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl Container for WasmContainer {
    fn get_data(&self) -> Result<ContainerData> {
        Ok(self.data.clone())
    }
}
