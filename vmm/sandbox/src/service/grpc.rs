/*
Copyright 2024 The Kuasar Authors.

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

//! gRPC service (HTTP/2 over Unix socket) on `--grpc-listen`.
//!
//! Two services are registered on the same socket:
//!   - `SandboxController`         — sandbox instance lifecycle (pause/resume/list/get)
//!   - `SandboxSnapshotController` — snapshot artifact lifecycle + plugin introspection (SSI)

use std::{os::unix::fs::PermissionsExt, sync::Arc};

use containerd_sandbox::SandboxStatus;
use log::{info, warn};
use tonic::{Request, Response, Status};
use vmm_api::{
    sandbox_grpc::{
        sandbox_controller_server::{SandboxController, SandboxControllerServer},
        GetSandboxRequest, GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse,
        PauseSandboxRequest, PauseSandboxResponse, ResumeSandboxRequest, ResumeSandboxResponse,
        Sandbox, SnapshotMode,
    },
    ssi_grpc::{
        sandbox_snapshot_controller_server::{
            SandboxSnapshotController, SandboxSnapshotControllerServer,
        },
        CreateSandboxSnapshotRequest, CreateSandboxSnapshotResponse, DeleteSandboxSnapshotRequest,
        DeleteSandboxSnapshotResponse, GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse,
        GetPluginInfoRequest, GetPluginInfoResponse, GetSandboxSnapshotRequest,
        GetSandboxSnapshotResponse, ListSandboxSnapshotsRequest, ListSandboxSnapshotsResponse,
        PluginCapability, PluginCapabilityType, ProbeRequest, ProbeResponse, SandboxSnapshot,
        SnapshotMode as SsiSnapshotMode,
    },
};

use super::{sandbox::resume_paused_sandbox, snapshot::snapshot_from_sandbox, Handle};
use crate::{
    sandbox::sandbox_pod_uid,
    template::{new_template_id, SnapshotType, TemplateKey, WorkloadIdentity},
    version::version_string,
    vm::{Snapshottable, VMFactory, VM},
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Shared state for both gRPC services.
pub struct GrpcHandle<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    pub inner: Handle<F>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn grpc_err(msg: impl std::fmt::Display) -> Status {
    Status::internal(msg.to_string())
}

/// Map SSI snapshot mode (FORK/RESUME) to the internal kuasar SnapshotType.
fn snapshot_type(mode: SsiSnapshotMode) -> SnapshotType {
    match mode {
        SsiSnapshotMode::Resume => SnapshotType::Continuation,
        _ => SnapshotType::WarmFork,
    }
}

/// Serialize a pooled template into an SSI SandboxSnapshot proto.
fn template_to_proto(
    tmpl: &crate::template::PooledTemplate,
    snapshot_name: &str,
    pod_uid: &str,
) -> SandboxSnapshot {
    let mode = match tmpl.snapshot_type {
        SnapshotType::WarmFork => SsiSnapshotMode::Fork as i32,
        SnapshotType::Continuation => SsiSnapshotMode::Resume as i32,
        _ => SsiSnapshotMode::Fork as i32,
    };
    SandboxSnapshot {
        pod_uid: pod_uid.to_string(),
        snapshot_name: snapshot_name.to_string(),
        mode,
    }
}

async fn find_sandbox_id_by_pod_uid<F>(handle: &Handle<F>, pod_uid: &str) -> Option<String>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let id = handle.pod_uid_index.read().await.get(pod_uid).cloned();
    if id.is_none() {
        warn!(
            "find_sandbox_id_by_pod_uid: no sandbox found for pod_uid={}",
            pod_uid
        );
    }
    id
}

fn sandbox_status_str(status: &SandboxStatus) -> &'static str {
    match status {
        SandboxStatus::Created => "created",
        SandboxStatus::Running(_) => "running",
        SandboxStatus::Stopped(_, _) => "stopped",
        SandboxStatus::Paused => "paused",
    }
}

// ---------------------------------------------------------------------------
// Newtype wrapper — shared by both service impls
// ---------------------------------------------------------------------------

pub struct GrpcHandleWrapper<F>(Arc<GrpcHandle<F>>)
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static;

impl<F> Clone for GrpcHandleWrapper<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    fn clone(&self) -> Self {
        GrpcHandleWrapper(Arc::clone(&self.0))
    }
}

// ---------------------------------------------------------------------------
// SandboxController — sandbox instance lifecycle
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl<F> SandboxController for GrpcHandleWrapper<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    async fn pause_sandbox(
        &self,
        request: Request<PauseSandboxRequest>,
    ) -> Result<Response<PauseSandboxResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }

        // Collect the pod_uid for WorkloadIdentity metadata before taking the snapshot.
        let pod_uid = {
            let sandboxes = handle.inner.sandboxes.read().await;
            let mtx = sandboxes
                .get(&req.sandbox_id)
                .ok_or_else(|| Status::not_found(format!("sandbox {} not found", req.sandbox_id)))?
                .clone();
            let sb = mtx.lock().await;
            sandbox_pod_uid(&sb).unwrap_or_else(|| req.sandbox_id.clone())
        };

        // 1. Snapshot into the continuation store.  The store key is the sandbox_id so
        //    resume_sandbox can look it up without additional metadata.
        //    snapshot_from_sandbox internally pauses vCPUs, writes files, then resumes the VM.
        let template_id = new_template_id();
        snapshot_from_sandbox(
            &handle.inner,
            &req.sandbox_id,
            &template_id,
            &req.sandbox_id,
            SnapshotType::Continuation,
            Some(WorkloadIdentity {
                pod_uid,
                generation: 0,
            }),
        )
        .await
        .map_err(grpc_err)?;

        // 2. Mark as Paused (before kill so the monitor skips the Stopped transition),
        //    then kill CH.  The snapshot is already on disk.
        let sandbox_mutex = handle
            .inner
            .sandboxes
            .read()
            .await
            .get(&req.sandbox_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("sandbox {} not found", req.sandbox_id)))?;

        let mut sandbox = sandbox_mutex.lock().await;
        sandbox.status = SandboxStatus::Paused;
        sandbox.vm.stop(true).await.map_err(grpc_err)?;
        sandbox.restore.template_key = Some(req.sandbox_id.clone());
        if let Err(e) = sandbox.dump().await {
            warn!(
                "pause_sandbox {}: dump after pause failed: {}",
                req.sandbox_id, e
            );
        }
        info!(
            "service:sandbox {} paused and snapshot saved",
            req.sandbox_id
        );

        Ok(Response::new(PauseSandboxResponse {}))
    }

    async fn resume_sandbox(
        &self,
        request: Request<ResumeSandboxRequest>,
    ) -> Result<Response<ResumeSandboxResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }

        resume_paused_sandbox(&handle.inner, &req.sandbox_id)
            .await
            .map_err(grpc_err)?;

        Ok(Response::new(ResumeSandboxResponse {}))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let handle = &self.0;
        let sandboxes_map = handle.inner.sandboxes.read().await;
        let mut sandboxes = Vec::with_capacity(sandboxes_map.len());
        for (id, mtx) in sandboxes_map.iter() {
            let sb = mtx.lock().await;
            let snapshot_mode = match sb.restore.template_snapshot_type.as_ref() {
                Some(SnapshotType::WarmFork) => SnapshotMode::WarmFork as i32,
                Some(SnapshotType::Continuation) => SnapshotMode::Continuation as i32,
                _ => SnapshotMode::Unspecified as i32,
            };
            let created_at_secs = sb
                .data
                .created_at
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            sandboxes.push(Sandbox {
                pod_uid: sandbox_pod_uid(&sb).unwrap_or_default(),
                sandbox_id: id.clone(),
                snapshot_name: sb.restore.template_key.clone().unwrap_or_default(),
                snapshot_mode,
                created_at_secs,
                status: sandbox_status_str(&sb.status).to_string(),
            });
        }
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.sandbox_id.is_empty() {
            return Err(Status::invalid_argument("sandbox_id is required"));
        }

        let sandboxes_map = handle.inner.sandboxes.read().await;
        let mtx = sandboxes_map
            .get(&req.sandbox_id)
            .ok_or_else(|| Status::not_found(format!("sandbox {} not found", req.sandbox_id)))?;
        let sb = mtx.lock().await;
        let snapshot_mode = match sb.restore.template_snapshot_type.as_ref() {
            Some(SnapshotType::WarmFork) => SnapshotMode::WarmFork as i32,
            Some(SnapshotType::Continuation) => SnapshotMode::Continuation as i32,
            _ => SnapshotMode::Unspecified as i32,
        };
        let created_at_secs = sb
            .data
            .created_at
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Response::new(GetSandboxResponse {
            sandbox: Some(Sandbox {
                pod_uid: sandbox_pod_uid(&sb).unwrap_or_default(),
                sandbox_id: sb.id.clone(),
                snapshot_name: sb.restore.template_key.clone().unwrap_or_default(),
                snapshot_mode,
                created_at_secs,
                status: sandbox_status_str(&sb.status).to_string(),
            }),
        }))
    }
}

// ---------------------------------------------------------------------------
// SandboxSnapshotController — SSI northbound API
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl<F> SandboxSnapshotController for GrpcHandleWrapper<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: "kuasar-vmm-snapshot".to_string(),
            version: version_string().to_string(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities: vec![
                PluginCapability {
                    r#type: PluginCapabilityType::PluginFork as i32,
                },
                PluginCapability {
                    r#type: PluginCapabilityType::PluginResume as i32,
                },
            ],
        }))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse { ready: true }))
    }

    async fn create_sandbox_snapshot(
        &self,
        request: Request<CreateSandboxSnapshotRequest>,
    ) -> Result<Response<CreateSandboxSnapshotResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.pod_uid.is_empty() {
            return Err(Status::invalid_argument("pod_uid is required"));
        }
        {
            let n = &req.snapshot_name;
            let has_traversal = n.contains('/') || n.split('/').any(|c| c == ".." || c == ".");
            if has_traversal {
                return Err(Status::invalid_argument(
                    "snapshot_name must not contain path traversal components ('/', '..', '.')",
                ));
            }
        }

        let sandbox_id = find_sandbox_id_by_pod_uid(&handle.inner, &req.pod_uid)
            .await
            .ok_or_else(|| {
                grpc_err(format!(
                    "no running sandbox found for pod_uid={}",
                    req.pod_uid
                ))
            })?;

        let mode = SsiSnapshotMode::from_i32(req.mode).unwrap_or(SsiSnapshotMode::Fork);
        let snap_type = snapshot_type(mode);

        let workload_identity = if matches!(snap_type, SnapshotType::Continuation) {
            let generation = match req.parameters.get("generation") {
                None => 0u64,
                Some(g) => g.parse::<u64>().map_err(|_| {
                    Status::invalid_argument(format!("invalid generation parameter: {:?}", g))
                })?,
            };
            Some(WorkloadIdentity {
                pod_uid: req.pod_uid.clone(),
                generation,
            })
        } else {
            None
        };

        let key = match &workload_identity {
            Some(wi) if req.snapshot_name.is_empty() => {
                TemplateKey::from_workload_identity(&wi.pod_uid, wi.generation).key
            }
            _ => {
                if req.snapshot_name.is_empty() {
                    return Err(Status::invalid_argument(
                        "snapshot_name is required for fork mode",
                    ));
                }
                req.snapshot_name.clone()
            }
        };

        let template_id = new_template_id();
        let tmpl = snapshot_from_sandbox(
            &handle.inner,
            &sandbox_id,
            &template_id,
            &key,
            snap_type,
            workload_identity,
        )
        .await
        .map_err(grpc_err)?;

        Ok(Response::new(CreateSandboxSnapshotResponse {
            snapshot: Some(template_to_proto(&tmpl, &key, &req.pod_uid)),
        }))
    }

    async fn delete_sandbox_snapshot(
        &self,
        request: Request<DeleteSandboxSnapshotRequest>,
    ) -> Result<Response<DeleteSandboxSnapshotResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.snapshot_name.is_empty() {
            return Err(Status::invalid_argument("snapshot_name is required"));
        }

        if let Some(pool) = &handle.inner.pool {
            let found = pool
                .list_templates()
                .await
                .into_iter()
                .find(|t| t.key.key == req.snapshot_name);
            if let Some(tmpl) = found {
                pool.remove_by_id(&tmpl.id, &SnapshotType::WarmFork)
                    .await
                    .map_err(grpc_err)?;
            }
        }
        if let Some(cs) = &handle.inner.continuation_store {
            let found = cs
                .list()
                .await
                .into_iter()
                .find(|t| t.key.key == req.snapshot_name);
            if let Some(tmpl) = found {
                cs.delete_by_template_id(&tmpl.id).await.map_err(grpc_err)?;
            }
        }

        Ok(Response::new(DeleteSandboxSnapshotResponse {}))
    }

    async fn list_sandbox_snapshots(
        &self,
        request: Request<ListSandboxSnapshotsRequest>,
    ) -> Result<Response<ListSandboxSnapshotsResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;
        let filter = SsiSnapshotMode::from_i32(req.mode).unwrap_or(SsiSnapshotMode::Unspecified);

        let mut snapshots: Vec<SandboxSnapshot> = Vec::new();

        if !matches!(filter, SsiSnapshotMode::Resume) {
            if let Some(pool) = &handle.inner.pool {
                for tmpl in pool.list_templates().await {
                    if matches!(tmpl.snapshot_type, SnapshotType::WarmFork) {
                        snapshots.push(template_to_proto(&tmpl, &tmpl.key.key.clone(), ""));
                    }
                }
            }
        }

        if !matches!(filter, SsiSnapshotMode::Fork) {
            if let Some(cs) = &handle.inner.continuation_store {
                for tmpl in cs.list().await {
                    let pod_uid = tmpl
                        .workload_identity
                        .as_ref()
                        .map(|wi| wi.pod_uid.as_str())
                        .unwrap_or("");
                    snapshots.push(template_to_proto(&tmpl, &tmpl.key.key.clone(), pod_uid));
                }
            }
        }

        Ok(Response::new(ListSandboxSnapshotsResponse { snapshots }))
    }

    async fn get_sandbox_snapshot(
        &self,
        request: Request<GetSandboxSnapshotRequest>,
    ) -> Result<Response<GetSandboxSnapshotResponse>, Status> {
        let req = request.into_inner();
        let handle = &self.0;

        if req.snapshot_name.is_empty() {
            return Err(Status::invalid_argument("snapshot_name is required"));
        }

        if let Some(pool) = &handle.inner.pool {
            let found = pool
                .list_templates()
                .await
                .into_iter()
                .find(|t| t.key.key == req.snapshot_name);
            if let Some(tmpl) = found {
                return Ok(Response::new(GetSandboxSnapshotResponse {
                    snapshot: Some(template_to_proto(&tmpl, &req.snapshot_name, "")),
                }));
            }
        }

        if let Some(cs) = &handle.inner.continuation_store {
            let found = cs
                .list()
                .await
                .into_iter()
                .find(|t| t.key.key == req.snapshot_name);
            if let Some(tmpl) = found {
                let pod_uid = tmpl
                    .workload_identity
                    .as_ref()
                    .map(|wi| wi.pod_uid.as_str())
                    .unwrap_or("");
                return Ok(Response::new(GetSandboxSnapshotResponse {
                    snapshot: Some(template_to_proto(&tmpl, &req.snapshot_name, pod_uid)),
                }));
            }
        }

        Err(Status::not_found(format!(
            "snapshot '{}' not found",
            req.snapshot_name
        )))
    }
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the gRPC server on a Unix domain socket.
///
/// Both `SandboxController` and `SandboxSnapshotController` are served on
/// the same socket. Removes any stale socket file before binding.
pub async fn serve<F>(handle: Arc<GrpcHandle<F>>, sock_path: &str) -> anyhow::Result<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    use tokio::net::UnixListener;
    use tokio_stream::wrappers::UnixListenerStream;

    let _ = std::fs::remove_file(sock_path);

    let uds = UnixListener::bind(sock_path)?;
    std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600))?;
    let uds_stream = UnixListenerStream::new(uds);

    let wrapper = GrpcHandleWrapper(handle);

    tonic::transport::Server::builder()
        .add_service(SandboxControllerServer::new(wrapper.clone()))
        .add_service(SandboxSnapshotControllerServer::new(wrapper))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}
