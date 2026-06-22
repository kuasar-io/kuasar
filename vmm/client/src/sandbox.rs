use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use vmm_api::sandbox_grpc::{
    sandbox_controller_client::SandboxControllerClient, GetSandboxRequest, ListSandboxesRequest,
    PauseSandboxRequest, ResumeSandboxRequest, Sandbox, SnapshotMode,
};

/// Sandbox summary returned by `list` and `get`.
#[derive(Debug)]
pub struct SandboxInfo {
    pub pod_uid: String,
    pub sandbox_id: String,
    pub snapshot_name: String,
    pub snapshot_mode: String,
    pub created_at_secs: i64,
    pub status: String,
}

/// Client for sandbox instance lifecycle (gRPC `SandboxController`).
pub struct SandboxApi {
    sock: PathBuf,
}

impl SandboxApi {
    pub fn new(sock: impl AsRef<Path>) -> Self {
        Self {
            sock: sock.as_ref().to_owned(),
        }
    }

    async fn connect(&self) -> Result<SandboxControllerClient<Channel>> {
        let sock = self.sock.clone();
        let channel = Endpoint::from_static("http://[::]:50051")
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = sock.clone();
                async move { UnixStream::connect(path).await }
            }))
            .await
            .with_context(|| format!("connect to gRPC socket {:?}", self.sock))?;
        Ok(SandboxControllerClient::new(channel))
    }

    /// Checkpoint a running sandbox to disk and stop the CH process.
    /// Network state (tap, TC rules) is preserved; call [`Self::resume`] to restore.
    pub async fn pause(&self, sandbox_id: &str) -> Result<()> {
        self.connect()
            .await?
            .pause_sandbox(PauseSandboxRequest {
                sandbox_id: sandbox_id.to_string(),
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?;
        Ok(())
    }

    /// Resume a previously paused sandbox VM.
    pub async fn resume(&self, sandbox_id: &str) -> Result<()> {
        self.connect()
            .await?
            .resume_sandbox(ResumeSandboxRequest {
                sandbox_id: sandbox_id.to_string(),
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?;
        Ok(())
    }

    /// List all sandbox instances on this node.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let resp = self
            .connect()
            .await?
            .list_sandboxes(ListSandboxesRequest {})
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();
        Ok(resp.sandboxes.into_iter().map(proto_to_info).collect())
    }

    /// Get details of a single sandbox by ID.
    pub async fn get(&self, sandbox_id: &str) -> Result<SandboxInfo> {
        let resp = self
            .connect()
            .await?
            .get_sandbox(GetSandboxRequest {
                sandbox_id: sandbox_id.to_string(),
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s))?
            .into_inner();
        resp.sandbox
            .map(proto_to_info)
            .ok_or_else(|| anyhow::anyhow!("empty sandbox in response"))
    }
}

fn proto_to_info(sb: Sandbox) -> SandboxInfo {
    let snapshot_mode =
        match SnapshotMode::from_i32(sb.snapshot_mode).unwrap_or(SnapshotMode::Unspecified) {
            SnapshotMode::WarmFork => "warm_fork",
            SnapshotMode::Continuation => "continuation",
            _ => "-",
        };
    SandboxInfo {
        pod_uid: sb.pod_uid,
        sandbox_id: sb.sandbox_id,
        snapshot_name: sb.snapshot_name,
        snapshot_mode: snapshot_mode.to_string(),
        created_at_secs: sb.created_at_secs,
        status: sb.status,
    }
}
