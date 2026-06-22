use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use crate::client::{call_admin, check_ok};

/// Client for template pool operations via the admin socket.
pub struct TemplateApi {
    admin_sock: PathBuf,
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
    pub queued: usize,
    pub in_flight: usize,
}

/// Result of a `pool-gc` operation.
#[derive(Debug, Serialize)]
pub struct GcResult {
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
    pub async fn refill(&self, target_depth: usize) -> Result<RefillResult> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "pool-refill",
                "target_depth": target_depth,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(RefillResult {
            queued: resp["queued"].as_u64().unwrap_or(0) as usize,
            in_flight: resp["in_flight"].as_u64().unwrap_or(0) as usize,
        })
    }

    /// Remove a single environment template from the pool by ID.
    pub async fn gc(&self, template_id: &str) -> Result<GcResult> {
        let resp = call_admin(
            self.sock(),
            json!({
                "action": "pool-gc",
                "template_id": template_id,
            }),
        )
        .await?;
        let resp = check_ok(resp)?;
        Ok(GcResult {
            removed: resp["removed"].as_u64().unwrap_or(0) as usize,
            remaining: resp["remaining"].as_u64().unwrap_or(0) as usize,
        })
    }
}
