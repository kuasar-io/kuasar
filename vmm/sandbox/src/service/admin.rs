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

//! Unix-socket admin API for template pool management and diagnostics.
//!
//! Listens on `--admin-listen` (default `/run/vmm-sandboxer-admin.sock`).
//! Protocol: one JSON object per line, one request/response per connection.
//!
//! Sandbox and snapshot lifecycle operations are handled by the gRPC service on
//! `--grpc-listen` (`/run/vmm-sandboxer-service.sock`).
//!
//! ## Template actions
//!
//! ### template-list / template-get — inspect available templates
//! ```json
//! {"action":"template-list"}
//! {"action":"template-get","template_id":"<id>"}
//! ```
//! Lists or returns available Environment/WarmFork templates from the pool and available
//! Continuation entries from the continuation store.
//!
//! ### pool-status — query pool health and metrics
//! ```json
//! {"action":"pool-status"}
//! ```
//!
//! ### pool-refill — spawn background environment-VM refill tasks
//! ```json
//! {"action":"pool-refill","kind":"environment","target_depth":3}
//! ```
//! - `kind`          required; only `"environment"` is supported
//! - `target_depth`  refill up to this many templates (existing + in-flight count toward the target)
//!
//! ### pool-gc — remove a single template from the pool by ID
//! ```json
//! {"action":"pool-gc","template_id":"<id>"}
//! ```
//! - `template_id`   ID of the template to remove (required)
//!
//! ## Response envelope
//! ```json
//! {"ok":true,...}
//! {"ok":false,"error":"<message>"}
//! ```

use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use anyhow::anyhow;
use log::{error, info, warn};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use super::Handle;
use crate::{
    sandbox::create_template_worker,
    template::{new_template_id, CreateTemplateRequest, PooledTemplate, SnapshotType, TemplateKey},
    vm::{Snapshottable, VMFactory, VM},
};

/// Listens on a Unix socket and dispatches template pool management commands.
pub struct Server<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    handle: Arc<Handle<F>>,
    sock_path: PathBuf,
}

impl<F> Server<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    pub fn new(handle: Handle<F>, sock_path: impl Into<PathBuf>) -> Self {
        Self {
            handle: Arc::new(handle),
            sock_path: sock_path.into(),
        }
    }

    /// Run the admin server, accepting connections until the process exits.
    pub async fn serve(self) {
        let _ = tokio::fs::remove_file(&self.sock_path).await;

        let listener = match UnixListener::bind(&self.sock_path) {
            Ok(l) => l,
            Err(e) => {
                error!("service:failed to bind {:?}: {}", self.sock_path, e);
                return;
            }
        };
        if let Err(e) =
            std::fs::set_permissions(&self.sock_path, std::fs::Permissions::from_mode(0o600))
        {
            warn!("service:could not restrict socket permissions: {}", e);
        }
        info!("service:listening on {:?}", self.sock_path);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let handle = self.handle.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(handle, stream).await {
                            warn!("service:connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("service:accept error: {}", e);
                }
            }
        }
    }
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    if id.len() < 12 || id.len() > 64 {
        return Err(anyhow!("id must be 12-64 characters, got {}", id.len()));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "id '{}' contains invalid characters (only [a-zA-Z0-9_-] allowed)",
            id
        ));
    }
    Ok(())
}

async fn write_json_response<W>(writer: &mut W, response: Value) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut out = serde_json::to_string(&response)?;
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    Ok(())
}

async fn handle_connection<F>(handle: Arc<Handle<F>>, stream: UnixStream) -> anyhow::Result<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("empty request"))?;

    let req: Value = serde_json::from_str(&line)?;
    let action = req["action"].as_str().unwrap_or("").to_string();

    let (category, verb) = action.split_once('-').unwrap_or((&action, ""));

    let result = match category {
        "template" => match verb {
            "list" => handle_template_list(&handle).await,
            "get" => handle_template_get(&handle, &req).await,
            _ => Err(anyhow!("unknown template action: template-{}", verb)),
        },
        "pool" => match verb {
            "status" => handle_pool_status(&handle).await,
            "refill" => handle_pool_refill(&handle, &req).await,
            "gc" => handle_pool_gc(&handle, &req).await,
            _ => Err(anyhow!("unknown pool action: pool-{}", verb)),
        },
        _ => Err(anyhow!("unknown action: {}", action)),
    };

    let response = result.unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
    write_json_response(&mut writer, response).await
}

async fn handle_pool_status<F>(handle: &Handle<F>) -> anyhow::Result<Value>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let pool = handle
        .pool
        .as_ref()
        .ok_or_else(|| anyhow!("template pool not configured"))?;
    let total = pool.total_depth().await;
    let keys = pool.key_count().await;
    let in_flight = pool.in_flight_count().await;
    let by_type = pool.depth_by_type().await;
    let by_key = pool.depth_by_key().await;
    let active_shared_refs = pool.active_shared_refs().await;
    let in_flight_restores = pool.in_flight_restores_by_template().await;
    let gc_blocked = pool.gc_blocked_templates().await;
    let continuation_count = if let Some(store) = &handle.continuation_store {
        store.list().await.len()
    } else {
        0
    };
    let m = &pool.metrics;
    Ok(json!({
        "ok": true,
        "total": total,
        "by_type": by_type.into_iter().map(|(t, d)| json!({"type": t, "count": d})).collect::<Vec<_>>(),
        "continuation": continuation_count,
        "keys": keys,
        "in_flight": in_flight,
        "by_key": by_key.into_iter().map(|(k, d)| json!({"key": k, "depth": d})).collect::<Vec<_>>(),
        "active_shared_refs": active_shared_refs.into_iter().map(|(id, refs)| json!({"template_id": id, "refs": refs})).collect::<Vec<_>>(),
        "in_flight_restores": in_flight_restores.into_iter().map(|(id, count)| json!({"template_id": id, "count": count})).collect::<Vec<_>>(),
        "gc_blocked": gc_blocked.into_iter().map(|(id, reason)| json!({"template_id": id, "reason": reason})).collect::<Vec<_>>(),
        "templates_created": m.templates_created.load(std::sync::atomic::Ordering::Relaxed),
        "pool_hits": m.pool_hits.load(std::sync::atomic::Ordering::Relaxed),
        "pool_misses": m.pool_misses.load(std::sync::atomic::Ordering::Relaxed),
        "hit_rate": m.hit_rate(),
        "avg_restore_ms": m.avg_restore_ms(),
    }))
}

fn snapshot_type_str(kind: &SnapshotType) -> &'static str {
    match kind {
        SnapshotType::Environment => "environment",
        SnapshotType::WarmFork => "warm_fork",
        SnapshotType::Continuation => "continuation",
    }
}

fn template_json(t: &PooledTemplate) -> Value {
    let mut item = json!({
        "template_id": t.id,
        "snapshot_type": snapshot_type_str(&t.snapshot_type),
        "key": t.key.key,
        "created_at_secs": t.created_at_secs,
        "snapshot_dir": t.snapshot_dir,
    });
    if let Some(wi) = &t.workload_identity {
        item["pod_uid"] = wi.pod_uid.clone().into();
        item["generation"] = wi.generation.into();
    }
    item
}

async fn handle_template_list<F>(handle: &Handle<F>) -> anyhow::Result<Value>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let mut entries = Vec::new();
    if let Some(pool) = &handle.pool {
        entries.extend(pool.list_templates().await.iter().map(template_json));
    }
    if let Some(store) = &handle.continuation_store {
        entries.extend(store.list().await.iter().map(template_json));
    }
    entries.sort_by(|a, b| {
        a["template_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["template_id"].as_str().unwrap_or(""))
    });
    Ok(json!({"ok": true, "templates": entries}))
}

async fn handle_template_get<F>(handle: &Handle<F>, req: &Value) -> anyhow::Result<Value>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let template_id = req["template_id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing template_id"))?;
    validate_id(template_id).map_err(|e| anyhow!("invalid template_id: {}", e))?;

    if let Some(pool) = &handle.pool {
        if let Some(t) = pool.get_template(template_id).await {
            return Ok(json!({"ok": true, "template": template_json(&t)}));
        }
    }
    if let Some(store) = &handle.continuation_store {
        if let Some(t) = store.list().await.into_iter().find(|t| t.id == template_id) {
            return Ok(json!({"ok": true, "template": template_json(&t)}));
        }
    }
    Err(anyhow!("template {} not found", template_id))
}

async fn handle_pool_refill<F>(handle: &Handle<F>, req: &Value) -> anyhow::Result<Value>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let pool = handle
        .pool
        .as_ref()
        .ok_or_else(|| anyhow!("template pool not configured"))?;
    let target_depth = req["target_depth"].as_u64().unwrap_or(1) as usize;
    let handle_policy = handle.factory.storage_policy();
    let key = TemplateKey::from_vm_config(
        handle.factory.kernel_path(),
        handle.factory.image_path(),
        handle.factory.vcpus(),
        handle.factory.memory_mb(),
        handle.factory.kernel_params(),
        &handle_policy.storage_backend,
    );
    let current = pool.depth(&key).await;
    let in_flight = pool.in_flight_count_for_key(&key).await;
    let need = target_depth.saturating_sub(current.saturating_add(in_flight));
    for _ in 0..need {
        let refill_id = new_template_id();
        let pool_c = pool.clone();
        let factory_c = Arc::clone(&handle.factory);
        pool.begin_refill(&key).await;
        let key_c = key.clone();
        tokio::spawn(async move {
            if let Err(e) = create_template_worker(
                factory_c,
                pool_c.clone(),
                CreateTemplateRequest::new_with_lease_mode(refill_id, pool_c.lease_mode.clone()),
            )
            .await
            {
                warn!("refill: template creation failed: {}", e);
            }
            pool_c.end_refill(&key_c).await;
        });
    }
    let new_in_flight = pool.in_flight_count_for_key(&key).await;
    Ok(json!({"ok": true, "queued": need, "in_flight": new_in_flight}))
}

async fn handle_pool_gc<F>(handle: &Handle<F>, req: &Value) -> anyhow::Result<Value>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let pool = handle
        .pool
        .as_ref()
        .ok_or_else(|| anyhow!("template pool not configured"))?;

    let template_id = match req["template_id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            return Ok(json!({"ok": false, "error": "missing required field 'template_id'"}));
        }
    };

    match pool
        .remove_by_id(&template_id, &SnapshotType::Environment)
        .await
    {
        Err(e) => Ok(json!({"ok": false, "error": e.to_string()})),
        Ok(false) => {
            Ok(json!({"ok": false, "error": format!("template {} not found in pool", template_id)}))
        }
        Ok(true) => {
            let remaining = pool.total_depth().await;
            Ok(json!({"ok": true, "removed": 1, "remaining": remaining}))
        }
    }
}
