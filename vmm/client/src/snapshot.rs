/*
Copyright 2026 The Kuasar Authors.

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

//! Client for the `SandboxSnapshotController` SSI gRPC service (HTTP/2 over Unix socket).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use vmm_api::ssi_grpc::{
    sandbox_snapshot_controller_client::SandboxSnapshotControllerClient,
    CreateSandboxSnapshotRequest, DeleteSandboxSnapshotRequest, GetPluginInfoRequest,
    GetSandboxSnapshotRequest, ListSandboxSnapshotsRequest, ProbeRequest, SnapshotMode,
};

/// Snapshot information returned by list/create.
#[derive(Debug)]
pub struct SnapshotInfo {
    pub pod_uid: String,
    pub snapshot_name: String,
    pub mode: String,
}

/// Plugin identity returned by `info`.
#[derive(Debug)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
}

/// Client for the `SandboxSnapshotController` gRPC service.
pub struct SnapshotApi {
    sock: PathBuf,
}

impl SnapshotApi {
    pub fn new(sock: impl AsRef<Path>) -> Self {
        Self {
            sock: sock.as_ref().to_owned(),
        }
    }

    async fn connect(&self) -> Result<SandboxSnapshotControllerClient<Channel>> {
        let sock = self.sock.clone();
        // tonic requires a valid URI even for Unix sockets; the host is ignored.
        let channel = Endpoint::from_static("http://[::]:50051")
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = sock.clone();
                async move { UnixStream::connect(path).await }
            }))
            .await
            .with_context(|| format!("connect to snapshot gRPC socket {:?}", self.sock))?;
        Ok(SandboxSnapshotControllerClient::new(channel))
    }

    /// Create a snapshot from a running pod (identified by `pod_uid`).
    ///
    /// `mode` is `"warm_fork"` or `"continuation"`.
    /// For Continuation, pass the workload `generation` (default 0).
    pub async fn create(
        &self,
        snapshot_name: &str,
        pod_uid: &str,
        mode: &str,
        generation: Option<u64>,
    ) -> Result<SnapshotInfo> {
        let mut parameters = std::collections::HashMap::new();
        if let Some(g) = generation {
            parameters.insert("generation".to_string(), g.to_string());
        }

        let req = CreateSandboxSnapshotRequest {
            snapshot_name: snapshot_name.to_string(),
            pod_uid: pod_uid.to_string(),
            mode: parse_mode(mode)? as i32,
            parameters,
        };

        let resp = self
            .connect()
            .await?
            .create_sandbox_snapshot(req)
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();

        let snap = resp
            .snapshot
            .ok_or_else(|| anyhow::anyhow!("empty snapshot in response"))?;
        Ok(proto_to_info(snap))
    }

    /// Get a snapshot by its `snapshot_name`. Returns `None` if not found.
    pub async fn get(&self, snapshot_name: &str) -> Result<Option<SnapshotInfo>> {
        let resp = self
            .connect()
            .await?
            .get_sandbox_snapshot(GetSandboxSnapshotRequest {
                snapshot_name: snapshot_name.to_string(),
            })
            .await;

        match resp {
            Ok(r) => {
                let snap = r
                    .into_inner()
                    .snapshot
                    .ok_or_else(|| anyhow::anyhow!("empty snapshot in response"))?;
                Ok(Some(proto_to_info(snap)))
            }
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(anyhow::anyhow!("{}", s)),
        }
    }

    /// Delete a snapshot by its `snapshot_name`.
    pub async fn delete(&self, snapshot_name: &str) -> Result<()> {
        self.connect()
            .await?
            .delete_sandbox_snapshot(DeleteSandboxSnapshotRequest {
                snapshot_name: snapshot_name.to_string(),
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?;
        Ok(())
    }

    /// List snapshots, optionally filtered by `mode` (`"warm_fork"` or `"continuation"`).
    /// Pass `None` to list all.
    pub async fn list(&self, mode: Option<&str>) -> Result<Vec<SnapshotInfo>> {
        let proto_mode = mode
            .map(parse_mode)
            .transpose()?
            .unwrap_or(SnapshotMode::Unspecified);

        let resp = self
            .connect()
            .await?
            .list_sandbox_snapshots(ListSandboxSnapshotsRequest {
                mode: proto_mode as i32,
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();

        Ok(resp.snapshots.into_iter().map(proto_to_info).collect())
    }

    /// Health-check the snapshot service.
    pub async fn probe(&self) -> Result<bool> {
        let resp = self
            .connect()
            .await?
            .probe(ProbeRequest {})
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();
        Ok(resp.ready)
    }

    /// Return plugin name and version.
    pub async fn info(&self) -> Result<PluginInfo> {
        let resp = self
            .connect()
            .await?
            .get_plugin_info(GetPluginInfoRequest {})
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();
        Ok(PluginInfo {
            name: resp.name,
            version: resp.version,
        })
    }
}

fn parse_mode(s: &str) -> Result<SnapshotMode> {
    match s {
        "warm_fork" | "warm-fork" => Ok(SnapshotMode::Fork),
        "continuation" => Ok(SnapshotMode::Resume),
        other => Err(anyhow::anyhow!(
            "unknown snapshot mode '{}'; use warm_fork or continuation",
            other
        )),
    }
}

fn proto_to_info(snap: vmm_api::ssi_grpc::SandboxSnapshot) -> SnapshotInfo {
    let mode = match SnapshotMode::from_i32(snap.mode).unwrap_or(SnapshotMode::Fork) {
        SnapshotMode::Fork => "warm_fork",
        SnapshotMode::Resume => "continuation",
        SnapshotMode::Unspecified => "warm_fork",
    };
    SnapshotInfo {
        pod_uid: snap.pod_uid,
        snapshot_name: snap.snapshot_name,
        mode: mode.to_string(),
    }
}
