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

use std::sync::Arc;

use async_trait::async_trait;
use containerd_shim::{
    api::*,
    error::Error,
    other,
    protos::{shim_async::TaskClient, ttrpc::context::with_timeout},
    DeleteResponse, Task, TtrpcContext, TtrpcResult,
};
use tokio::sync::{Mutex, RwLock};

use crate::{io::ContainerIoTransport, service::KuasarServer};

// cheap to clone
#[derive(Clone)]
pub struct TaskHandler {
    pub task_cli: Arc<Mutex<Option<TaskClient>>>,
    pub task_addr: Arc<RwLock<String>>,
}

impl TaskHandler {
    async fn state(&self, ctx: &TtrpcContext, req: StateRequest) -> TtrpcResult<StateResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.state(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn create(
        &self,
        ctx: &TtrpcContext,
        req: CreateTaskRequest,
    ) -> TtrpcResult<CreateTaskResponse> {
        // Call Task.Create with new request.
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.create(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn start(&self, ctx: &TtrpcContext, req: &StartRequest) -> TtrpcResult<StartResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.start(with_timeout(ctx.timeout_nano), req).await
    }

    async fn delete(&self, ctx: &TtrpcContext, req: &DeleteRequest) -> TtrpcResult<DeleteResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.delete(with_timeout(ctx.timeout_nano), req).await
    }

    async fn pids(&self, ctx: &TtrpcContext, req: PidsRequest) -> TtrpcResult<PidsResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.pids(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn pause(&self, ctx: &TtrpcContext, req: PauseRequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.pause(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn resume(&self, ctx: &TtrpcContext, req: ResumeRequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.resume(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn checkpoint(
        &self,
        ctx: &TtrpcContext,
        req: CheckpointTaskRequest,
    ) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli
            .checkpoint(with_timeout(ctx.timeout_nano), &req)
            .await
    }

    async fn kill(&self, ctx: &TtrpcContext, req: KillRequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.kill(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn exec(&self, ctx: &TtrpcContext, req: ExecProcessRequest) -> TtrpcResult<Empty> {
        // Call Task.Exec lastly
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.exec(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn resize_pty(&self, ctx: &TtrpcContext, req: ResizePtyRequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli
            .resize_pty(with_timeout(ctx.timeout_nano), &req)
            .await
    }

    async fn close_io(&self, ctx: &TtrpcContext, req: CloseIORequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli
            .close_io(with_timeout(ctx.timeout_nano), &req)
            .await
    }

    async fn update(&self, ctx: &TtrpcContext, req: UpdateTaskRequest) -> TtrpcResult<Empty> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.update(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn wait(&self, ctx: &TtrpcContext, req: &WaitRequest) -> TtrpcResult<WaitResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.wait(with_timeout(ctx.timeout_nano), req).await
    }

    async fn stats(&self, ctx: &TtrpcContext, req: StatsRequest) -> TtrpcResult<StatsResponse> {
        let task_cli = self
            .task_cli
            .lock()
            .await
            .clone()
            .ok_or_else(|| other!("FATAL: task_cli for vm task server is not initialized!"))?;
        task_cli.stats(with_timeout(ctx.timeout_nano), &req).await
    }

    async fn connect(
        &self,
        _ctx: &TtrpcContext,
        _: ConnectRequest,
    ) -> TtrpcResult<ConnectResponse> {
        Ok(ConnectResponse {
            shim_pid: std::process::id(),
            //clh pid as task_pid
            ..Default::default()
        })
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _: ShutdownRequest) -> TtrpcResult<Empty> {
        Ok(Empty::new())
    }
}

#[async_trait]
impl<T> Task for KuasarServer<T>
where
    T: ContainerIoTransport,
{
    async fn state(&self, ctx: &TtrpcContext, req: StateRequest) -> TtrpcResult<StateResponse> {
        self.task.state(ctx, req).await
    }

    async fn create(
        &self,
        ctx: &TtrpcContext,
        req: CreateTaskRequest,
    ) -> TtrpcResult<CreateTaskResponse> {
        // create task shim io
        let task_addr = self.task.task_addr.read().await.clone();
        let shim_io = T::new(
            task_addr,
            req.stdin.clone(),
            req.stdout.clone(),
            req.stderr.clone(),
            req.terminal,
        )
        .await?;
        // Call Controller.AppendContainer previously, so need type structure conversion at first.
        let resp_v2 = self.sandbox.prepare_container(&shim_io, &req).await?;
        let prepare_res = resp_v2.get_ref();

        // Update rootfs and io for new Task.Create request.
        let mut req_new = req.clone();

        req_new.stdin = shim_io.container_in();
        req_new.stdout = shim_io.container_out();
        req_new.stderr = shim_io.container_err();

        if !prepare_res.bundle.is_empty() {
            req_new.bundle = prepare_res.bundle.clone();
        }

        let res = self.task.create(ctx, req_new).await?;
        // Copy io after init process cloned lastly.
        shim_io.copy().await?;
        Ok(res)
    }

    async fn start(&self, ctx: &TtrpcContext, req: StartRequest) -> TtrpcResult<StartResponse> {
        let res = self.task.start(ctx, &req).await?;

        // Copy io after exec process executed lastly.
        if !req.exec_id.is_empty() {
            let sandbox_guard = self.sandbox.data.lock().await;
            let process_data = sandbox_guard
                .get_container_data(&req.id)?
                .get_process_data(&req.exec_id)?;
            process_data.io.copy().await?;
        }
        Ok(res)
    }

    async fn delete(&self, ctx: &TtrpcContext, req: DeleteRequest) -> TtrpcResult<DeleteResponse> {
        let res = self.task.delete(ctx, &req).await?;

        let mut sandbox_guard = self.sandbox.data.lock().await;
        if let Ok(container_data) = sandbox_guard.get_mut_container_data(&req.id) {
            if req.exec_id.is_empty() {
                // Remove container from sandbox struct.
                T::cleanup_connection(container_data.io.clone()).await;
                sandbox_guard.delete_container_data(&req.id);
            } else if let Ok(process_data) = container_data.get_process_data(&req.exec_id) {
                T::cleanup_connection(process_data.io.clone()).await;
                container_data.delete_process_data(&req.exec_id)
            }
        }

        // Remove container.
        let _resp_v2 = self.sandbox.purge(&req.id, &req.exec_id).await?;
        Ok(res)
    }

    async fn pids(&self, ctx: &TtrpcContext, req: PidsRequest) -> TtrpcResult<PidsResponse> {
        self.task.pids(ctx, req).await
    }

    async fn pause(&self, ctx: &TtrpcContext, req: PauseRequest) -> TtrpcResult<Empty> {
        self.task.pause(ctx, req).await
    }

    async fn resume(&self, ctx: &TtrpcContext, req: ResumeRequest) -> TtrpcResult<Empty> {
        self.task.resume(ctx, req).await
    }

    async fn checkpoint(
        &self,
        ctx: &TtrpcContext,
        req: CheckpointTaskRequest,
    ) -> TtrpcResult<Empty> {
        self.task.checkpoint(ctx, req).await
    }

    async fn kill(&self, ctx: &TtrpcContext, req: KillRequest) -> TtrpcResult<Empty> {
        self.task.kill(ctx, req).await
    }

    async fn exec(&self, ctx: &TtrpcContext, req: ExecProcessRequest) -> TtrpcResult<Empty> {
        // create task shim io
        let task_addr = self.task.task_addr.read().await.clone();
        let shim_io = T::new(
            task_addr,
            req.stdin.clone(),
            req.stdout.clone(),
            req.stderr.clone(),
            req.terminal,
        )
        .await?;

        // update container
        let resp_v2 = self.sandbox.prepare_exec(&shim_io, &req).await?;
        let _prepare_resp = resp_v2.get_ref();

        // Update io and process spec in new Task.Exec request
        let mut req_new = req.clone();

        req_new.stdin = shim_io.container_in();
        req_new.stdout = shim_io.container_out();
        req_new.stderr = shim_io.container_err();

        self.task.exec(ctx, req_new).await
    }

    async fn resize_pty(&self, ctx: &TtrpcContext, req: ResizePtyRequest) -> TtrpcResult<Empty> {
        self.task.resize_pty(ctx, req).await
    }

    async fn close_io(&self, ctx: &TtrpcContext, req: CloseIORequest) -> TtrpcResult<Empty> {
        self.task.close_io(ctx, req).await
    }

    async fn update(&self, ctx: &TtrpcContext, req: UpdateTaskRequest) -> TtrpcResult<Empty> {
        self.task.update(ctx, req).await
    }

    async fn wait(&self, ctx: &TtrpcContext, req: WaitRequest) -> TtrpcResult<WaitResponse> {
        self.task.wait(ctx, &req).await
    }

    async fn stats(&self, ctx: &TtrpcContext, req: StatsRequest) -> TtrpcResult<StatsResponse> {
        self.task.stats(ctx, req).await
    }

    async fn connect(
        &self,
        ctx: &TtrpcContext,
        req: ConnectRequest,
    ) -> TtrpcResult<ConnectResponse> {
        self.task.connect(ctx, req).await
    }

    async fn shutdown(&self, ctx: &TtrpcContext, req: ShutdownRequest) -> TtrpcResult<Empty> {
        self.task.shutdown(ctx, req).await
    }
}
