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

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use containerd_sandbox::api::sandbox::v1::controller_client::ControllerClient as SandboxV2Client;
use containerd_shim::{
    error::Result,
    protos::{
        protobuf::MessageField,
        sandbox::{
            sandbox::{ShutdownSandboxRequest, StopSandboxRequest},
            sandbox_ttrpc::Sandbox,
        },
    },
    publisher::RemotePublisher,
    spawn,
    util::write_str_to_file,
    Config, DeleteResponse, ExitSignal, Shim, StartOpts, TtrpcContext,
};
use log::warn;
use tokio::{
    net::UnixStream,
    sync::{Mutex, RwLock},
};
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

use crate::{
    data::SandboxData, io::ContainerIoTransport, sandbox::SandboxHandler, task::TaskHandler,
};

pub struct Service<T> {
    kuasar_server: Box<KuasarServer<T>>,
}

#[async_trait]
impl<Transport> Shim for Service<Transport>
where
    Transport: ContainerIoTransport,
{
    type T = KuasarServer<Transport>;

    async fn new(_runtime_id: &str, id: &str, _namespace: &str, _config: &mut Config) -> Self {
        let exit = Arc::new(ExitSignal::default());
        Self {
            kuasar_server: Box::new(KuasarServer::new(id, exit).await),
        }
    }

    async fn start_shim(&mut self, opts: StartOpts) -> Result<String> {
        let grouping = opts.id.clone();
        let address = spawn(opts, &grouping, Vec::new()).await?;
        write_str_to_file("address", &address).await?;
        Ok(address)
    }

    async fn delete_shim(&mut self) -> Result<DeleteResponse> {
        // If shim process exited unexpectedly, containerd will call shim delete command
        // to clean up sandbox. So shim should call sandboxer for stopping and deleting.
        // Otherwise, nobody will delete this sandbox.
        let ctx = TtrpcContext {
            fd: 0,
            mh: Default::default(),
            metadata: Default::default(),
            timeout_nano: 0,
        };
        let stop_req = StopSandboxRequest {
            sandbox_id: self.kuasar_server.sandbox.id(),
            timeout_secs: 15,
            ..Default::default()
        };
        self.kuasar_server
            .stop_sandbox(&ctx, stop_req)
            .await
            .unwrap_or_else(|e| {
                warn!("failed to call sandboxer to stop: {}", e);
                Default::default()
            });

        let delete_req = ShutdownSandboxRequest {
            sandbox_id: self.kuasar_server.sandbox.id(),
            ..Default::default()
        };
        self.kuasar_server
            .shutdown_sandbox(&ctx, delete_req)
            .await
            .unwrap_or_else(|e| {
                warn!("failed to call sandboxer to delete: {}", e);
                Default::default()
            });

        let resp = DeleteResponse {
            pid: 0,
            exit_status: 137,
            exited_at: MessageField::from(Some(SystemTime::now().into())),
            ..Default::default()
        };
        Ok(resp)
    }

    async fn wait(&mut self) {
        self.kuasar_server.exit.wait().await;
    }

    async fn create_task_service(&self, _publisher: RemotePublisher) -> Self::T {
        *self.kuasar_server.clone()
    }

    type S = KuasarServer<Transport>;

    async fn create_sandbox_service(&self) -> Self::S {
        *self.kuasar_server.clone()
    }
}

#[derive(Clone)]
pub struct KuasarServer<T> {
    pub sandbox: SandboxHandler<T>,
    pub task: TaskHandler,
    pub exit: Arc<ExitSignal>,
}

impl<T: ContainerIoTransport> KuasarServer<T> {
    pub async fn new(id: &str, exit: Arc<ExitSignal>) -> Self {
        let channel = Endpoint::from_static("https://www.kuasar.io")
            .connect_with_connector(service_fn(
                |_: Uri| UnixStream::connect(T::sandboxer_addr()),
            ))
            .await
            .expect("sandboxer should be running");

        Self {
            sandbox: SandboxHandler {
                id: id.to_string(),
                data: Arc::new(Mutex::new(SandboxData::default())),
                sandbox_v2_cli: SandboxV2Client::new(channel),
            },
            task: TaskHandler {
                task_cli: Arc::new(Mutex::new(None)),
                task_addr: Arc::new(RwLock::new("".to_string())),
            },
            exit,
        }
    }

    pub async fn init_task_client(&self, task_addr: &str) -> Result<()> {
        let mut addr_guard = self.task.task_addr.write().await;
        *addr_guard = task_addr.to_string();

        let mut task_guard = self.task.task_cli.lock().await;

        let client = T::new_task_client(task_addr).await?;
        *task_guard = Some(client);

        Ok(())
    }
}
