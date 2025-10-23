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

use std::{collections::HashMap, ops::BitOr, path::Path, sync::Arc, time::SystemTime};

use async_trait::async_trait;
use containerd_sandbox::{
    api::sandbox::v1 as v2,
    data::{ProcessResource, TaskResource, TaskResources},
    spec::{JsonSpec, Process},
    types::Sandbox,
    utils::unmount,
};
use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest},
    error::{Error, Result},
    io_error, other_error,
    protos::{
        protobuf::{well_known_types, MessageField},
        sandbox::{sandbox::*, sandbox_ttrpc::Sandbox as SandboxTrait},
        ttrpc::{self as Ttrpc, error::get_rpc_status},
        types::platform::Platform,
    },
    util::read_spec,
    TtrpcContext, TtrpcResult,
};
use log::{info, warn};
use nix::mount::{mount, MsFlags};
use tokio::{
    fs::{copy, create_dir_all, remove_dir_all, File},
    sync::Mutex,
};
use tonic::transport::Channel;

use crate::{
    data::{ContainerData, ProcessData, SandboxData},
    io::ContainerIoTransport,
    service::KuasarServer,
};

pub(crate) const VMM_SANDBOXER_SOCKET_PATH: &str = "/run/vmm-sandboxer.sock";
pub(crate) const WASM_SANDBOXER_SOCKET_PATH: &str = "/run/wasm-sandboxer.sock";
pub(crate) const CRI_SANDBOX_ROOT_PATH: &str =
    "/var/lib/containerd/io.containerd.grpc.v1.cri/sandboxes/";
pub(crate) const CRI_SANDBOX_STATE_PATH: &str =
    "/run/containerd/io.containerd.grpc.v1.cri/sandboxes/";

pub fn grpc_to_ttrpc(status: tonic::Status) -> containerd_shim::protos::ttrpc::error::Error {
    containerd_shim::protos::ttrpc::error::Error::RpcStatus(
        containerd_shim::protos::ttrpc::Status {
            code: containerd_shim::protos::protobuf::EnumOrUnknown::from_i32(status.code().into()),
            message: status.message().to_string(),
            ..Default::default()
        },
    )
}

#[async_trait]
impl<T> SandboxTrait for KuasarServer<T>
where
    T: ContainerIoTransport,
{
    async fn create_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: CreateSandboxRequest,
    ) -> TtrpcResult<CreateSandboxResponse> {
        self.sandbox.create_sandbox(_ctx, req).await
    }

    async fn start_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: StartSandboxRequest,
    ) -> TtrpcResult<StartSandboxResponse> {
        let (task_address, resp) = self.sandbox.start_sandbox(_ctx, req).await?;

        // Connect to VM task-server
        self.init_task_client(&task_address).await?;

        Ok(resp)
    }

    async fn platform(
        &self,
        _ctx: &TtrpcContext,
        _: PlatformRequest,
    ) -> TtrpcResult<PlatformResponse> {
        let pf = Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: "".to_string(),
            ..Default::default()
        };
        Ok(PlatformResponse {
            platform: MessageField::from(Some(pf)),
            ..Default::default()
        })
    }

    async fn stop_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: StopSandboxRequest,
    ) -> TtrpcResult<StopSandboxResponse> {
        self.sandbox.stop_sandbox(_ctx, req).await
    }

    async fn wait_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: WaitSandboxRequest,
    ) -> TtrpcResult<WaitSandboxResponse> {
        self.sandbox.wait_sandbox(_ctx, req).await
    }

    async fn sandbox_status(
        &self,
        _ctx: &TtrpcContext,
        req: SandboxStatusRequest,
    ) -> TtrpcResult<SandboxStatusResponse> {
        self.sandbox.sandbox_status(_ctx, req).await
    }

    async fn ping_sandbox(&self, _ctx: &TtrpcContext, _: PingRequest) -> TtrpcResult<PingResponse> {
        Err(get_rpc_status(
            Ttrpc::Code::UNIMPLEMENTED,
            "/containerd.runtime.sandbox.v1.Sandbox/PingSandbox is not supported".to_string(),
        ))
    }

    async fn shutdown_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: ShutdownSandboxRequest,
    ) -> TtrpcResult<ShutdownSandboxResponse> {
        let res = self.sandbox.shutdown_sandbox(_ctx, req).await?;

        self.exit.signal();
        Ok(res)
    }
}

#[derive(Clone)]
pub struct SandboxHandler<T> {
    pub id: String,
    pub data: Arc<Mutex<SandboxData<T>>>,
    pub sandbox_v2_cli: v2::controller_client::ControllerClient<Channel>,
}

impl<T> SandboxHandler<T>
where
    T: ContainerIoTransport,
{
    pub async fn prepare_container(&self, shim_io: &T, req: &CreateTaskRequest) -> TtrpcResult<()> {
        let id = req.id.to_string();
        let spec = read_spec::<JsonSpec>(&req.bundle).await?;

        let rootfs = req
            .rootfs
            .iter()
            .map(|x| containerd_sandbox::spec::Mount {
                destination: "".to_string(),
                r#type: x.type_.to_string(),
                source: x.source.to_string(),
                options: x.options.clone(),
            })
            .collect();
        let mut tasks = self.get_tasks_extension().await?;
        let task_resource = TaskResource {
            task_id: req.id.clone(),
            spec: Some(spec),
            rootfs,
            stdin: req.stdin.clone(),
            stdout: req.stdout.clone(),
            stderr: req.stderr.clone(),
            processes: vec![],
        };
        tasks.tasks.push(task_resource);
        self.update_tasks_extension(tasks).await?;

        // Append container data to sandbox structure.
        let mut sandbox_guard = self.data.lock().await;
        let container_data = ContainerData::new(&id, shim_io.clone());
        sandbox_guard.add_container_data(container_data);
        Ok(())
    }

    pub async fn prepare_exec(&self, shim_io: &T, req: &ExecProcessRequest) -> TtrpcResult<()> {
        let id = req.id.to_string();
        let exec_id = req.exec_id.to_string();

        let mut task_resources = self.get_tasks_extension().await?;
        for t in task_resources.tasks.iter_mut() {
            if t.task_id == id {
                let spec: Process =
                    serde_json::from_slice(&req.spec.value).map_err(other_error!(e, ""))?;
                t.processes.push(ProcessResource {
                    exec_id: exec_id.clone(),
                    spec: Some(spec),
                    stdin: shim_io.container_in(),
                    stdout: shim_io.container_out(),
                    stderr: shim_io.container_err(),
                });
            }
        }
        self.update_tasks_extension(task_resources).await?;
        // Append process data to the container, only update process field, we don't care of other
        // fields, as the server doesn't need them either.
        // Call this before Controller.UpdateContainer because UpdateContainer need the container
        // loaded its whole process, while Exec request does not contain other processes. So we
        // need get all process_data then append this data. This can be fix then Controller has
        // implemented AppendProcess as I think.
        let mut sandbox_guard = self.data.lock().await;
        let process_data = ProcessData::new(&exec_id, shim_io.clone());
        let container_data = sandbox_guard.get_mut_container_data(&id)?;
        container_data.add_process_data(process_data);
        Ok(())
    }

    pub async fn purge(&self, container_id: &str, exec_id: &str) -> TtrpcResult<()> {
        let mut task_resources = self.get_tasks_extension().await?;
        task_resources.tasks.retain_mut(|t| {
            if t.task_id == container_id {
                if exec_id.is_empty() {
                    // Remove the whole task for the container.
                    return false;
                }
                // Remove just the specified process from the task.
                t.processes.retain(|p| p.exec_id != exec_id);
            }
            // Keep the task.
            true
        });
        self.update_tasks_extension(task_resources).await?;
        let mut sandbox_guard = self.data.lock().await;
        if exec_id.is_empty() {
            sandbox_guard.delete_container_data(container_id);
        } else {
            let container_data = sandbox_guard.get_mut_container_data(container_id)?;
            container_data.delete_process_data(exec_id);
        }

        Ok(())
    }

    pub async fn create_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: CreateSandboxRequest,
    ) -> TtrpcResult<CreateSandboxResponse> {
        // TODO: When setupSandboxFiles was merged into containerd, it can be removed then.
        self.setup_sandbox_files().await?;

        let options = req.options.into_option().map(|any| prost_types::Any {
            type_url: any.type_url,
            value: any.value,
        });

        let mut data_guard = self.data.lock().await;
        *data_guard = SandboxData {
            sandbox: Sandbox {
                sandbox_id: req.sandbox_id,
                runtime: None,
                spec: None,
                labels: HashMap::new(),
                created_at: Some(SystemTime::now().into()),
                updated_at: Some(SystemTime::now().into()),
                extensions: HashMap::new(),
                sandboxer: "".to_string(),
            },
            containers: Default::default(),
        };

        let req_v2 = v2::ControllerCreateRequest {
            sandboxer: "".to_string(),
            sandbox_id: self.id(),
            rootfs: vec![],
            options,
            netns_path: req.netns_path.to_string(),
            annotations: HashMap::new(),
            sandbox: None,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let _resp_v2 = cli.create(req_v2).await.map_err(grpc_to_ttrpc)?;

        let resp = CreateSandboxResponse::new();
        Ok(resp)
    }

    pub async fn start_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: StartSandboxRequest,
    ) -> TtrpcResult<(String, StartSandboxResponse)> {
        if req.sandbox_id != self.id {
            return Err(get_rpc_status(
                Ttrpc::Code::INVALID_ARGUMENT,
                "the requested sandbox id doesn't match current shim",
            ));
        }

        let req_v2 = v2::ControllerStartRequest {
            sandboxer: "".to_string(),
            sandbox_id: self.id(),
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let resp_v2 = cli.start(req_v2).await.map_err(grpc_to_ttrpc)?;
        let start_resp = resp_v2.get_ref().clone();

        let created_at = &start_resp.created_at.and_then(|ts| {
            SystemTime::try_from(ts)
                .ok()
                .map(well_known_types::timestamp::Timestamp::from)
        });

        let resp = StartSandboxResponse {
            pid: start_resp.pid,
            created_at: MessageField::from(created_at.clone()),
            ..Default::default()
        };

        let req_v2 = v2::ControllerStatusRequest {
            sandboxer: "".to_string(),
            sandbox_id: req.sandbox_id,
            verbose: false,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let resp_v2 = cli.status(req_v2).await.map_err(grpc_to_ttrpc)?;
        let status_resp = resp_v2.get_ref();

        Ok((status_resp.address.to_string(), resp))
    }

    pub async fn stop_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: StopSandboxRequest,
    ) -> TtrpcResult<StopSandboxResponse> {
        if req.sandbox_id != self.id {
            return Err(get_rpc_status(
                Ttrpc::Code::INVALID_ARGUMENT,
                "the requested sandbox id doesn't match current shim",
            ));
        }

        let req_v2 = v2::ControllerStopRequest {
            sandboxer: "".to_string(),
            sandbox_id: req.sandbox_id,
            timeout_secs: req.timeout_secs,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let _resp_v2 = cli.stop(req_v2).await.map_err(grpc_to_ttrpc)?;

        let resp = StopSandboxResponse::new();
        Ok(resp)
    }

    pub async fn wait_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: WaitSandboxRequest,
    ) -> TtrpcResult<WaitSandboxResponse> {
        if req.sandbox_id != self.id {
            return Err(get_rpc_status(
                Ttrpc::Code::INVALID_ARGUMENT,
                "the requested sandbox id doesn't match current shim",
            ));
        }

        let req_v2 = v2::ControllerWaitRequest {
            sandboxer: "".to_string(),
            sandbox_id: req.sandbox_id,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let resp_v2 = cli.wait(req_v2).await.map_err(grpc_to_ttrpc)?;
        let wait_resp = resp_v2.get_ref();

        let exited_at = wait_resp.exited_at.to_owned().and_then(|ts| {
            SystemTime::try_from(ts)
                .ok()
                .map(well_known_types::timestamp::Timestamp::from)
        });

        let resp = WaitSandboxResponse {
            exit_status: wait_resp.exit_status,
            exited_at: MessageField::from(exited_at),
            ..Default::default()
        };
        Ok(resp)
    }

    pub async fn sandbox_status(
        &self,
        _ctx: &TtrpcContext,
        req: SandboxStatusRequest,
    ) -> TtrpcResult<SandboxStatusResponse> {
        info!("shim sandbox status");
        if req.sandbox_id != self.id {
            return Err(get_rpc_status(
                Ttrpc::Code::INVALID_ARGUMENT,
                "the requested sandbox id doesn't match current shim",
            ));
        }

        let req_v2 = v2::ControllerStatusRequest {
            sandboxer: "".to_string(),
            sandbox_id: req.sandbox_id,
            verbose: req.verbose,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let resp_v2 = cli.status(req_v2).await.map_err(grpc_to_ttrpc)?;
        let status_resp = resp_v2.get_ref();

        let resp = SandboxStatusResponse {
            sandbox_id: status_resp.sandbox_id.to_string(),
            pid: status_resp.pid,
            state: status_resp.state.to_string(),
            ..Default::default()
        };
        Ok(resp)
    }

    pub async fn shutdown_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: ShutdownSandboxRequest,
    ) -> TtrpcResult<ShutdownSandboxResponse> {
        if req.sandbox_id != self.id {
            return Err(get_rpc_status(
                Ttrpc::Code::INVALID_ARGUMENT,
                "the requested sandbox id doesn't match current shim",
            ));
        }

        let req_v2 = v2::ControllerShutdownRequest {
            sandboxer: "".to_string(),
            sandbox_id: req.sandbox_id,
        };
        let mut cli = self.sandbox_v2_cli.clone();
        let _resp_v2 = cli.shutdown(req_v2).await.map_err(|e| {
            warn!("failed to shutdown {}", e);
            grpc_to_ttrpc(e)
        });
        self.cleanup_sandbox_files()
            .await
            .unwrap_or_else(|e| warn!("failed to umount shm: {}", e));

        let resp = ShutdownSandboxResponse::new();
        Ok(resp)
    }
}

impl<T> SandboxHandler<T> {
    pub(crate) fn id(&self) -> String {
        self.id.to_string()
    }

    // TODO: need improvement in containerd. copy from containerd
    async fn setup_sandbox_files(&self) -> Result<()> {
        let id = self.id();
        let sandbox_path = Path::new(CRI_SANDBOX_ROOT_PATH).join(&id);
        create_dir_all(sandbox_path)
            .await
            .map_err(io_error!(e, "create sandbox root dir"))?;

        // handle hostname
        let hostname_path = Path::new(CRI_SANDBOX_ROOT_PATH).join(&id).join("hostname");
        File::create(hostname_path)
            .await
            .map_err(io_error!(e, "create hostname"))?;

        // handle hosts
        let hosts_path = Path::new(CRI_SANDBOX_ROOT_PATH).join(&id).join("hosts");
        copy("/etc/hosts", hosts_path)
            .await
            .map_err(io_error!(e, "copy hosts"))?;

        // handle resolv.conf
        let resolv_path = Path::new(CRI_SANDBOX_ROOT_PATH)
            .join(&id)
            .join("resolv.conf");
        copy("/etc/resolv.conf", resolv_path)
            .await
            .map_err(io_error!(e, "copy resolv.conf"))?;

        // handle /dev/shim
        let shm_path = Path::new(CRI_SANDBOX_STATE_PATH).join(&id).join("shm");
        create_dir_all(shm_path.clone())
            .await
            .map_err(io_error!(e, "create sandbox state dir"))?;
        let data = format!("mode=1777,size={}", 1024 * 1024 * 64);
        let flags = MsFlags::MS_NOEXEC
            .bitor(MsFlags::MS_NOSUID)
            .bitor(MsFlags::MS_NODEV);
        mount(
            Some("shm"),
            shm_path.to_str().unwrap(),
            Some("tmpfs"),
            flags,
            Some(data.as_str()),
        )
        .map_err(other_error!(e, "mount shm"))?;

        Ok(())
    }

    async fn cleanup_sandbox_files(&self) -> Result<()> {
        let id = self.id();

        let sandbox_path = Path::new(CRI_SANDBOX_ROOT_PATH).join(&id);
        remove_dir_all(sandbox_path).await.unwrap_or_default();

        let shm_path = Path::new(CRI_SANDBOX_STATE_PATH).join(&id).join("shm");
        // MNT_DETACH is 0x2
        unmount(shm_path.to_str().unwrap(), 2).map_err(other_error!(e, "umount shm"))?;

        remove_dir_all(shm_path).await.unwrap_or_default();

        Ok(())
    }

    async fn get_tasks_extension(&self) -> Result<TaskResources> {
        let sandbox = self.data.lock().await.sandbox.clone();
        let tasks = match sandbox.extensions.get("tasks") {
            Some(tasks_any) => serde_json::from_slice::<TaskResources>(&tasks_any.value)?,
            None => TaskResources { tasks: vec![] },
        };
        Ok(tasks)
    }

    async fn update_tasks_extension(&self, task_resources: TaskResources) -> Result<()> {
        let mut sandbox = self.data.lock().await.sandbox.clone();
        let tasks_bytes = serde_json::to_vec(&task_resources)?;
        let any = prost_types::Any {
            type_url: "github.com/containerd/containerd.TaskResources".to_string(),
            value: tasks_bytes,
        };
        sandbox.extensions.insert("tasks".to_string(), any);
        let req = v2::ControllerUpdateRequest {
            sandbox_id: sandbox.sandbox_id.clone(),
            sandboxer: sandbox.sandboxer.clone(),
            sandbox: Some(sandbox),
            fields: vec!["extensions.tasks".to_string()],
        };
        self.update(req).await?;
        Ok(())
    }

    async fn update(
        &self,
        req: v2::ControllerUpdateRequest,
    ) -> TtrpcResult<tonic::Response<v2::ControllerUpdateResponse>> {
        let mut sandbox_v2_cli = self.sandbox_v2_cli.clone();
        sandbox_v2_cli.update(req).await.map_err(grpc_to_ttrpc)
    }
}
