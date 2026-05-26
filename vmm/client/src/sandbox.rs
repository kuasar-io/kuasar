use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::client::{call_admin, check_ok};

/// Client for sandbox lifecycle operations.
pub struct SandboxApi {
    admin_sock: PathBuf,
}

/// Result of a successful `run` operation.
#[derive(Debug, Serialize)]
pub struct RunResult {
    pub sandbox_id: String,
    pub template_id: String,
}

/// Sandbox summary returned by `list` and `get`.
#[derive(Debug, Serialize)]
pub struct SandboxInfo {
    pub id: String,
    pub status: String,
    pub base_dir: String,
    pub template_id: Option<String>,
    pub template_snapshot_type: Option<String>,
    /// Only present in `get` responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_mode: Option<String>,
    /// Only present in `get` responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_restore_mode: Option<String>,
}

impl SandboxApi {
    pub fn new(admin_sock: impl Into<PathBuf>) -> Self {
        Self {
            admin_sock: admin_sock.into(),
        }
    }

    fn sock(&self) -> &Path {
        &self.admin_sock
    }

    /// Create a sandbox slot and restore a WarmFork snapshot.
    ///
    /// `key` selects the latest template for that pool key.
    /// `template_id` pins to a specific template by ID.
    /// When both are provided they must match.
    /// At least one of `key` or `template_id` must be `Some`.
    pub async fn run_warm_fork(
        &self,
        key: Option<&str>,
        template_id: Option<&str>,
    ) -> Result<RunResult> {
        let mut payload = json!({
            "action": "sandbox-run",
            "snapshot_type": "warm_fork",
        });
        if let Some(k) = key {
            payload["template_key"] = k.into();
        }
        if let Some(tid) = template_id {
            payload["template_id"] = tid.into();
        }
        let resp = call_admin(self.sock(), payload).await?;
        let resp = check_ok(resp)?;
        Ok(RunResult {
            sandbox_id: resp["sandbox_id"].as_str().unwrap_or("").to_string(),
            template_id: resp["template_id"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Create a sandbox slot and restore a Continuation snapshot by workload identity.
    ///
    /// The network identity must be migrated externally to the target node before calling this.
    pub async fn run_continuation(&self, pod_uid: &str, generation: u32) -> Result<RunResult> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "sandbox-run",
                "snapshot_type": "continuation",
                "pod_uid": pod_uid,
                "generation": generation,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(RunResult {
            sandbox_id: resp["sandbox_id"].as_str().unwrap_or("").to_string(),
            template_id: resp["template_id"].as_str().unwrap_or("").to_string(),
        })
    }

    /// List all sandboxes known to the sandboxer.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>> {
        let resp = call_admin(self.sock(), json!({"action": "sandbox-list"})).await?;
        let resp = check_ok(resp)?;
        let items = resp["sandboxes"]
            .as_array()
            .map(|arr| arr.iter().map(sandbox_info_from_value).collect())
            .unwrap_or_default();
        Ok(items)
    }

    /// Get details of a single sandbox by ID.
    pub async fn get(&self, sandbox_id: &str) -> Result<SandboxInfo> {
        let resp = call_admin(
            self.sock(),
            json!({"action": "sandbox-get", "sandbox_id": sandbox_id}),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(sandbox_info_from_value(&resp))
    }

    /// Stop a running sandbox, release its template lease, and delete all files.
    pub async fn destroy(&self, sandbox_id: &str) -> Result<()> {
        let resp = call_admin(
            self.sock(),
            json!({"action": "sandbox-destroy", "sandbox_id": sandbox_id}),
        )
        .await?;
        check_ok(resp)?;
        Ok(())
    }
}

fn sandbox_info_from_value(v: &serde_json::Value) -> SandboxInfo {
    SandboxInfo {
        id: v["id"].as_str().unwrap_or("").to_string(),
        status: v["status"].as_str().unwrap_or("").to_string(),
        base_dir: v["base_dir"].as_str().unwrap_or("").to_string(),
        template_id: v["template_id"].as_str().map(str::to_string),
        template_snapshot_type: v["template_snapshot_type"].as_str().map(str::to_string),
        lease_mode: v["lease_mode"].as_str().map(str::to_string),
        memory_restore_mode: v["memory_restore_mode"].as_str().map(str::to_string),
    }
}
