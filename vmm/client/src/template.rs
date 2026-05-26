use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use crate::client::{call_admin, check_ok};

/// Client for template and pool operations.
pub struct TemplateApi {
    admin_sock: PathBuf,
}

/// Result of a successful `template-create`.
#[derive(Debug, Serialize)]
pub struct TemplateInfo {
    pub template_id: String,
    pub key: String,
}

/// Raw pool-status payload returned by the sandboxer.
///
/// Wraps the full JSON object so callers can pretty-print or inspect any field.
#[derive(Debug, Serialize)]
pub struct PoolStatus(pub Value);

/// Raw template payload returned by `template-list` and `template-get`.
#[derive(Debug, Serialize)]
pub struct TemplateRecord(pub Value);

/// Result of a `pool-refill` operation.
#[derive(Debug, Serialize)]
pub struct RefillResult {
    pub kind: String,
    pub queued: usize,
    pub in_flight: usize,
}

/// Result of a `pool-gc` operation.
#[derive(Debug, Serialize)]
pub struct GcResult {
    pub kind: String,
    pub removed: usize,
    pub remaining: usize,
}

impl TemplateApi {
    pub fn new(admin_sock: impl Into<PathBuf>) -> Self {
        Self {
            admin_sock: admin_sock.into(),
        }
    }

    fn sock(&self) -> &Path {
        &self.admin_sock
    }

    /// Snapshot a running sandbox's VM and register it as a template.
    ///
    /// The sandbox continues running after the snapshot (VM is resumed immediately).
    /// The server generates the template ID.
    /// `key` is required: pods with `kuasar.io/template-key=<key>` will be matched
    /// to this template at start time.
    pub async fn create_from_sandbox(&self, sandbox_id: &str, key: &str) -> Result<TemplateInfo> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "template-create",
                "sandbox_id": sandbox_id,
                "key": key,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(TemplateInfo {
            template_id: resp["template_id"].as_str().unwrap_or("").to_string(),
            key: resp["key"].as_str().unwrap_or(key).to_string(),
        })
    }

    /// Query the template pool status and metrics.
    pub async fn pool_status(&self) -> Result<PoolStatus> {
        let resp = call_admin(self.sock(), json!({"action": "pool-status"})).await?;
        let resp = check_ok(resp)?;
        Ok(PoolStatus(resp))
    }

    /// List available templates across the template pool and continuation store.
    pub async fn list(&self) -> Result<Vec<TemplateRecord>> {
        let resp = call_admin(self.sock(), json!({"action": "template-list"})).await?;
        let resp = check_ok(resp)?;
        let items = resp["templates"]
            .as_array()
            .map(|arr| arr.iter().cloned().map(TemplateRecord).collect())
            .unwrap_or_default();
        Ok(items)
    }

    /// Get a single available template by server-generated template ID.
    pub async fn get(&self, template_id: &str) -> Result<TemplateRecord> {
        let resp = call_admin(
            self.sock(),
            json!({"action": "template-get", "template_id": template_id}),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(TemplateRecord(resp["template"].clone()))
    }

    /// Spawn background refill tasks to bring the pool up to `target_depth`.
    ///
    /// `kind` is `"environment"`, `"warm_fork"`, or `"continuation"`.
    pub async fn refill(&self, kind: &str, target_depth: usize) -> Result<RefillResult> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "pool-refill",
                "kind": kind,
                "target_depth": target_depth,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(RefillResult {
            kind: resp["kind"].as_str().unwrap_or(kind).to_string(),
            queued: resp["queued"].as_u64().unwrap_or(0) as usize,
            in_flight: resp["in_flight"].as_u64().unwrap_or(0) as usize,
        })
    }

    /// Snapshot a running sandbox's VM as a Continuation template.
    ///
    /// Unlike WarmFork, the sandbox does not need to be in ready-waiting state.
    /// The server generates the template ID.
    /// The pool key is auto-derived from `pod_uid` and `generation` on the server side.
    pub async fn create_continuation_from_sandbox(
        &self,
        sandbox_id: &str,
        pod_uid: &str,
        generation: u32,
    ) -> Result<TemplateInfo> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "template-create",
                "sandbox_id": sandbox_id,
                "snapshot_type": "continuation",
                "pod_uid": pod_uid,
                "generation": generation,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(TemplateInfo {
            template_id: resp["template_id"].as_str().unwrap_or("").to_string(),
            key: resp["key"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Remove a single template from the pool by ID.
    ///
    /// `kind` is `"environment"` or `"warm_fork"` and must match the template's actual kind
    /// (safety cross-check). `template_id` is the server-generated template ID.
    pub async fn gc(&self, kind: &str, template_id: &str) -> Result<GcResult> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "pool-gc",
                "kind": kind,
                "template_id": template_id,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(GcResult {
            kind: resp["kind"].as_str().unwrap_or(kind).to_string(),
            removed: resp["removed"].as_u64().unwrap_or(0) as usize,
            remaining: resp["remaining"].as_u64().unwrap_or(0) as usize,
        })
    }
}
