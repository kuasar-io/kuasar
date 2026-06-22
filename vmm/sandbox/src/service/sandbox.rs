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

use std::{collections::HashMap, io::ErrorKind, path::PathBuf, sync::Arc};

use anyhow::anyhow;
use containerd_sandbox::{
    data::SandboxData, error::Result, signal::ExitSignal, utils::cleanup_mounts, SandboxOption,
    SandboxStatus,
};
use log::{info, warn};
use tokio::{
    fs::{create_dir_all, remove_dir_all},
    sync::Mutex,
};

use super::Handle;
use crate::{
    cgroup::SandboxCgroup,
    sandbox::{
        monitor, sandbox_pod_uid, KuasarSandbox, MemoryRestoreMode, RestoreMetadata,
        TemplateLeaseMode,
    },
    storage::device_graph::DeviceGraph,
    template::{SnapshotType, TemplateKey, TemplateLease, WorkloadIdentity, POD_UID_ANNOTATION},
    vm::{RestoreSource, SnapshotPathOverrides, Snapshottable, VMFactory, WarmForkParams, VM},
};

/// Create a bare sandbox slot in `SandboxStatus::Created` state without going through containerd.
///
/// `pod_uid`, when provided, is stored as `kuasar.io/pod-uid` in the sandbox labels so that
/// subsequent `CreateSandboxSnapshot` calls can resolve it back to this sandbox.
pub async fn create_sandbox_slot<F>(
    handle: &Handle<F>,
    sandbox_id: &str,
    pod_uid: Option<&str>,
    netns: &str,
) -> Result<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    if handle.sandboxes.read().await.contains_key(sandbox_id) {
        return Err(anyhow!("sandbox {} already exists", sandbox_id).into());
    }
    let base_dir = handle.sandbox_base_dir.join(sandbox_id);
    create_dir_all(&base_dir)
        .await
        .map_err(|e| anyhow!("create sandbox dir {:?}: {}", base_dir, e))?;
    let base_dir_str = base_dir.to_string_lossy().to_string();
    let sandbox_opt = SandboxOption {
        base_dir: base_dir_str.clone(),
        sandbox: SandboxData {
            id: sandbox_id.to_string(),
            ..Default::default()
        },
    };
    let vm = handle
        .factory
        .create_vm(sandbox_id, &sandbox_opt)
        .await
        .map_err(|e| anyhow!("create_vm for sandbox slot {}: {}", sandbox_id, e))?;
    let mut labels = HashMap::new();
    if let Some(uid) = pod_uid {
        labels.insert(POD_UID_ANNOTATION.to_string(), uid.to_string());
    }
    let sandbox = KuasarSandbox {
        vm,
        id: sandbox_id.to_string(),
        status: SandboxStatus::Created,
        base_dir: base_dir_str,
        data: SandboxData {
            id: sandbox_id.to_string(),
            netns: netns.to_string(),
            labels,
            ..Default::default()
        },
        containers: Default::default(),
        storages: DeviceGraph::default(),
        id_generator: 0,
        network: None,
        client: Arc::new(Mutex::new(None)),
        exit_signal: Arc::new(ExitSignal::default()),
        sandbox_cgroups: SandboxCgroup::default(),
        storage_policy: handle.factory.storage_policy(),
        restore: RestoreMetadata::default(),
    };
    sandbox.setup_sandbox_files().await?;
    sandbox.dump().await?;
    let uid = sandbox_pod_uid(&sandbox);
    handle
        .sandboxes
        .write()
        .await
        .insert(sandbox_id.to_string(), Arc::new(Mutex::new(sandbox)));
    if let Some(uid) = uid {
        let mut index = handle.pod_uid_index.write().await;
        if let Some(old_id) = index.insert(uid.clone(), sandbox_id.to_string()) {
            if old_id != sandbox_id {
                warn!(
                    "pod_uid_index: pod_uid={} was mapped to {}, now replaced by {}",
                    uid, old_id, sandbox_id
                );
            }
        }
    }
    info!("service:created sandbox slot {} for run", sandbox_id);
    Ok(())
}

/// Stop a running sandbox, release its template lease, and delete its directory.
///
/// Works for sandboxes created by the gRPC service without containerd involvement.
pub async fn destroy_sandbox<F>(handle: &Handle<F>, sandbox_id: &str) -> Result<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .cloned()
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?;

    let base_dir;
    let consumed_template_id;
    let template_snapshot_type;
    let lease_mode;
    let memory_restore_mode;

    {
        let mut sandbox = sandbox_mutex.lock().await;
        base_dir = sandbox.base_dir.clone();
        consumed_template_id = sandbox.restore.template_id.clone();
        template_snapshot_type = sandbox.restore.template_snapshot_type.clone();
        lease_mode = sandbox.restore.lease_mode.clone();
        memory_restore_mode = sandbox.restore.memory_restore_mode.clone();

        sandbox
            .stop(true)
            .await
            .map_err(|e| anyhow!("stop sandbox {}: {}", sandbox_id, e))?;

        // Admin-created sandboxes have an empty cgroup_parent_path; skip cgroup removal.
        if !sandbox.sandbox_cgroups.cgroup_parent_path.is_empty()
            && !cgroups_rs::hierarchies::is_cgroup2_unified_mode()
        {
            if let Err(e) = sandbox.sandbox_cgroups.remove_sandbox_cgroups() {
                warn!("service:destroy {}: remove cgroups: {}", sandbox_id, e);
            }
        }
    }

    cleanup_mounts(&base_dir).await?;
    if let Err(e) = remove_dir_all(&base_dir).await {
        if e.kind() != ErrorKind::NotFound {
            return Err(anyhow!("remove sandbox dir {}: {}", base_dir, e).into());
        }
    }

    if let (Some(tid), Some(pool)) = (consumed_template_id, handle.pool.as_ref()) {
        match template_snapshot_type.as_ref() {
            Some(SnapshotType::Environment) => {
                if !matches!(
                    memory_restore_mode.as_ref(),
                    Some(MemoryRestoreMode::Copy) | None
                ) {
                    pool.deref(&tid).await;
                }
            }
            Some(SnapshotType::WarmFork) => match lease_mode {
                Some(TemplateLeaseMode::Shared) => pool.deref(&tid).await,
                // Exclusive/None: consumed marker present but snapshot files are retained;
                // cleanup_consumed is intentionally skipped here.
                _ => {}
            },
            Some(SnapshotType::Continuation) => {
                // Continuation snapshots are managed by ContinuationStore; no pool cleanup needed.
            }
            None => {}
        }
    }

    handle.sandboxes.write().await.remove(sandbox_id);
    handle
        .pod_uid_index
        .write()
        .await
        .retain(|_, sid| sid != sandbox_id);
    info!("service:destroyed sandbox {}", sandbox_id);
    Ok(())
}

/// Restore an already-created sandbox from a WarmFork pool template.
///
/// `key` selects the latest template for that pool key; `template_id` pins to a
/// specific template by ID.  When both are provided they must match.
/// Runs in autonomous mode (`task_id = None`): the ready-waiting process self-starts
/// after COMMIT without an external injection.
pub async fn restore_sandbox_warm_fork<F>(
    handle: &Handle<F>,
    sandbox_id: &str,
    key: Option<&str>,
    template_id: Option<&str>,
) -> Result<String>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?
        .clone();

    let mut sandbox = sandbox_mutex.lock().await;
    if !matches!(sandbox.status, SandboxStatus::Created) {
        return Err(anyhow!(
            "restore requires sandbox {} to be in Created state, current: {:?}",
            sandbox_id,
            sandbox.status
        )
        .into());
    }

    let pool = handle
        .pool
        .as_ref()
        .ok_or_else(|| anyhow!("template pool not configured"))?;

    // Acquire the template: by ID (with optional key verification) or by key.
    let (tmpl, acquired_template_id, lease_mode, lease) = if let Some(tid) = template_id {
        let raw = pool
            .acquire_by_id_for_restore(tid)
            .await
            .ok_or_else(|| anyhow!("WarmFork template '{}' not available for restore", tid))?;
        if raw.snapshot_type != SnapshotType::WarmFork {
            return Err(anyhow!(
                "template '{}' has snapshot_type {:?}; expected warm_fork",
                tid,
                raw.snapshot_type
            )
            .into());
        }
        let lm = pool.lease_mode.clone();
        let lease = TemplateLease::new(pool.clone(), raw.clone());
        let t = match lease.template() {
            Ok(t) => t.clone(),
            Err(e) => {
                lease.fail().await;
                return Err(e);
            }
        };
        if let Some(k) = key {
            let expected = TemplateKey::user(k);
            if t.key.key != expected.key {
                lease.fail().await;
                return Err(anyhow!(
                    "warm_fork template '{}' key '{}' does not match requested key '{}'",
                    tid,
                    t.key.key,
                    k
                )
                .into());
            }
        }
        let id = t.id.clone();
        (t, id, lm, lease)
    } else {
        let k = key
            .ok_or_else(|| anyhow!("restore_sandbox_warm_fork: key or template_id is required"))?;
        let tkey = TemplateKey::user(k);
        let raw = pool
            .acquire_for_restore(&tkey)
            .await
            .ok_or_else(|| anyhow!("no WarmFork template available for key '{}'", k))?;
        if raw.snapshot_type != SnapshotType::WarmFork {
            return Err(anyhow!(
                "template for key '{}' has snapshot_type {:?}; expected WarmFork",
                k,
                raw.snapshot_type
            )
            .into());
        }
        let lm = pool.lease_mode.clone();
        let id = raw.id.clone();
        let lease = TemplateLease::new(pool.clone(), raw.clone());
        let t = lease.template()?.clone();
        (t, id, lm, lease)
    };
    let base_dir = sandbox.base_dir.clone();
    let work_dir = PathBuf::from(format!("{}/restore", base_dir));
    let memory_restore_mode = handle.snapshot_config.default_memory_restore_mode.clone();

    // Autonomous mode: task_id = None, so the ready-waiting process self-starts after COMMIT.
    // Targets carry the snapshot-time container_id already; no overlay step needed.
    let warm_fork_params = WarmForkParams {
        task_id: None,
        targets: tmpl.warm_fork_targets.clone(),
        prepare_timeout_ms: 10_000,
        commit_timeout_ms: 5_000,
    };

    if !sandbox.data.netns.is_empty() {
        if let Err(e) = sandbox.prepare_network().await {
            lease.fail().await;
            sandbox.destroy_network().await;
            return Err(e);
        }
    }

    sandbox.restore.template_id = Some(acquired_template_id.clone());
    sandbox.restore.template_key = Some(tmpl.key.key.clone());
    sandbox.restore.template_snapshot_type = Some(SnapshotType::WarmFork);
    sandbox.restore.lease_mode = Some(lease_mode.clone());
    sandbox.restore.reflink_supported = Some(pool.reflink_supported);
    sandbox.id_generator = tmpl.id_generator;

    let src = RestoreSource {
        snapshot_dir: tmpl.snapshot_dir,
        work_dir,
        overrides: SnapshotPathOverrides::from_original(
            &base_dir,
            &tmpl.original_task_vsock,
            sandbox_id,
        ),
        lease_mode,
        snapshot_type: SnapshotType::WarmFork,
        disk_images: tmpl.disk_images,
        storages: tmpl.storages,
        orphan_container_ids: tmpl.orphan_container_ids,
        memory_restore_mode,
        reflink_supported: pool.reflink_supported,
        warm_fork_params: Some(warm_fork_params),
    };

    if let Err(e) = sandbox.start_from_snapshot(src).await {
        sandbox.clear_template_restore_state();
        sandbox.destroy_network().await;
        lease.fail().await;
        return Err(e);
    }

    monitor(sandbox_mutex.clone());

    if !sandbox.sandbox_cgroups.cgroup_parent_path.is_empty() {
        if let Err(e) = sandbox.add_to_cgroup().await {
            sandbox.clear_template_restore_state();
            if let Err(re) = sandbox.stop(true).await {
                warn!(
                    "service:sandbox {} rollback add_to_cgroup after WarmFork restore: {}",
                    sandbox_id, re
                );
            }
            sandbox.destroy_network().await;
            lease.fail().await;
            return Err(e);
        }
    }

    if let Err(e) = sandbox.dump().await {
        sandbox.clear_template_restore_state();
        if let Err(re) = sandbox.stop(true).await {
            warn!(
                "service:sandbox {} rollback dump after WarmFork restore: {}",
                sandbox_id, re
            );
        }
        sandbox.destroy_network().await;
        lease.fail().await;
        return Err(e);
    }

    lease.complete().await;
    info!(
        "service:sandbox {} restored from WarmFork template {} (key={}, template_id={})",
        sandbox_id,
        acquired_template_id,
        key.unwrap_or("-"),
        template_id.unwrap_or("-"),
    );
    Ok(acquired_template_id)
}

/// Restore an already-created sandbox from a Continuation snapshot by workload identity.
///
/// Two network paths are supported, selected by whether `sandbox.data.netns` is set
/// at entry (i.e. the caller passed `netns` to `create_sandbox_slot`):
///
/// * **Rebuild-network** (`data.netns` non-empty): a fresh CNI-allocated netns is
///   provided by the caller.  `prepare_network()` creates a new tap+veth in that netns
///   and `refresh_instance_identity()` pushes the new IP/routes into the guest — the
///   same path as WarmFork and the containerd Continuation path.
///
/// * **Preserved-netns fallback** (`data.netns` empty): the IFF_PERSIST tap from the
///   original pod's preserved netns is reopened via `reopen_continuation_taps()`.
///   The guest keeps its frozen IP; the operator is responsible for external routing.
pub async fn restore_sandbox_continuation<F>(
    handle: &Handle<F>,
    sandbox_id: &str,
    pod_uid: &str,
    generation: u64,
    snapshot_name: Option<&str>,
) -> Result<String>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?
        .clone();

    let mut sandbox = sandbox_mutex.lock().await;
    if !matches!(sandbox.status, SandboxStatus::Created) {
        return Err(anyhow!(
            "restore requires sandbox {} to be in Created state, current: {:?}",
            sandbox_id,
            sandbox.status
        )
        .into());
    }

    let store = handle
        .continuation_store
        .as_ref()
        .ok_or_else(|| anyhow!("continuation store not configured"))?;

    let acquired_key = snapshot_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}/g{}", pod_uid, generation));
    let lease = if let Some(name) = snapshot_name {
        store.acquire_by_key(name).await.ok_or_else(|| {
            anyhow!(
                "no continuation snapshot found for snapshot_name='{}'",
                name
            )
        })?
    } else {
        let identity = WorkloadIdentity {
            pod_uid: pod_uid.to_string(),
            generation,
        };
        store.acquire(&identity).await.ok_or_else(|| {
            anyhow!(
                "no continuation snapshot available for pod_uid='{}' generation={}",
                pod_uid,
                generation
            )
        })?
    };

    let tmpl = lease.template()?.clone();
    let acquired_template_id = tmpl.id.clone();
    let base_dir = sandbox.base_dir.clone();
    let work_dir = PathBuf::from(format!("{}/restore", base_dir));
    let memory_restore_mode = handle.snapshot_config.default_memory_restore_mode.clone();
    let reflink_supported = handle
        .pool
        .as_ref()
        .map(|p| p.reflink_supported)
        .unwrap_or(false);

    sandbox.restore.template_id = Some(acquired_template_id.clone());
    sandbox.restore.template_key = Some(acquired_key.clone());
    sandbox.restore.template_snapshot_type = Some(SnapshotType::Continuation);
    sandbox.restore.lease_mode = Some(TemplateLeaseMode::Exclusive);
    sandbox.restore.reflink_supported = Some(reflink_supported);
    sandbox.id_generator = tmpl.id_generator;

    // Rebuild-network path: caller pre-allocated a fresh CNI netns (set via create_sandbox_slot).
    // Preserved-netns fallback: no caller netns → fall back to the bind-mounted preserved netns
    // from the template so reopen_continuation_taps can find the IFF_PERSIST tap.
    let caller_provided_netns = !sandbox.data.netns.is_empty();
    if !caller_provided_netns && !tmpl.netns.is_empty() {
        sandbox.data.netns = tmpl.netns.clone();
    }

    let src = RestoreSource {
        snapshot_dir: tmpl.snapshot_dir,
        work_dir,
        overrides: SnapshotPathOverrides::from_original(
            &base_dir,
            &tmpl.original_task_vsock,
            sandbox_id,
        ),
        lease_mode: TemplateLeaseMode::Exclusive,
        snapshot_type: SnapshotType::Continuation,
        disk_images: tmpl.disk_images,
        storages: tmpl.storages,
        orphan_container_ids: tmpl.orphan_container_ids,
        memory_restore_mode,
        reflink_supported,
        warm_fork_params: None,
    };

    if !sandbox.data.netns.is_empty() {
        if caller_provided_netns {
            // Rebuild-network: create a fresh tap+veth in the caller's netns, just like
            // WarmFork.  refresh_instance_identity() will push the new IP into the guest.
            if let Err(e) = sandbox.prepare_network().await {
                sandbox.destroy_network().await;
                lease.fail().await;
                return Err(e);
            }
        } else {
            // Preserved-netns fallback: reopen the IFF_PERSIST tap with correct flags so
            // CH uses from_tap_fds (avoids IFF_VNET_HDR mismatch from Tap::open_named).
            let config_json = src.snapshot_dir.join("config.json");
            if let Err(e) = sandbox.reopen_continuation_taps(&config_json).await {
                lease.fail().await;
                return Err(e);
            }
        }
    }

    if let Err(e) = sandbox.start_from_snapshot(src).await {
        sandbox.clear_template_restore_state();
        if caller_provided_netns {
            sandbox.destroy_network().await;
        }
        lease.fail().await;
        return Err(e);
    }

    monitor(sandbox_mutex.clone());

    if !sandbox.sandbox_cgroups.cgroup_parent_path.is_empty() {
        if let Err(e) = sandbox.add_to_cgroup().await {
            sandbox.clear_template_restore_state();
            if let Err(re) = sandbox.stop(true).await {
                warn!(
                    "service:sandbox {} rollback add_to_cgroup after continuation restore: {}",
                    sandbox_id, re
                );
            }
            lease.fail().await;
            return Err(e);
        }
    }

    if let Err(e) = sandbox.dump().await {
        sandbox.clear_template_restore_state();
        if let Err(re) = sandbox.stop(true).await {
            warn!(
                "service:sandbox {} rollback dump after continuation restore: {}",
                sandbox_id, re
            );
        }
        lease.fail().await;
        return Err(e);
    }

    lease.complete().await;
    info!(
        "service:sandbox {} restored from continuation snapshot key={} (template {})",
        sandbox_id, acquired_key, acquired_template_id
    );
    Ok(acquired_template_id)
}

/// Resume a sandbox that was paused via `pause_sandbox`.
///
/// The continuation snapshot is keyed by `sandbox_id`.  The preserved netns
/// (bind-mounted during pause) is reused directly via `reopen_continuation_taps`
/// — no CNI / `prepare_network` is involved, so this only works on the same node.
pub async fn resume_paused_sandbox<F>(handle: &Handle<F>, sandbox_id: &str) -> Result<()>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?
        .clone();

    let mut sandbox = sandbox_mutex.lock().await;
    if !matches!(sandbox.status, SandboxStatus::Paused) {
        return Err(anyhow!(
            "resume requires sandbox {} to be in Paused state, current: {:?}",
            sandbox_id,
            sandbox.status
        )
        .into());
    }

    let store = handle
        .continuation_store
        .as_ref()
        .ok_or_else(|| anyhow!("continuation store not configured"))?;

    // pause_sandbox stores the snapshot keyed by sandbox_id
    let lease = store
        .acquire_by_key(sandbox_id)
        .await
        .ok_or_else(|| anyhow!("no pause snapshot found for sandbox {}", sandbox_id))?;

    let tmpl = lease.template()?.clone();
    let base_dir = sandbox.base_dir.clone();
    let work_dir = PathBuf::from(format!("{}/restore", base_dir));
    let memory_restore_mode = handle.snapshot_config.default_memory_restore_mode.clone();
    let reflink_supported = handle
        .pool
        .as_ref()
        .map(|p| p.reflink_supported)
        .unwrap_or(false);

    sandbox.restore.template_id = Some(tmpl.id.clone());
    sandbox.restore.template_key = Some(sandbox_id.to_string());
    sandbox.restore.template_snapshot_type = Some(SnapshotType::Continuation);
    sandbox.restore.lease_mode = Some(TemplateLeaseMode::Exclusive);
    sandbox.restore.reflink_supported = Some(reflink_supported);
    sandbox.id_generator = tmpl.id_generator;

    // Use the preserved netns bind-mounted during pause (netns path inside the store entry dir)
    if !tmpl.netns.is_empty() {
        sandbox.data.netns = tmpl.netns.clone();
    }

    let src = RestoreSource {
        snapshot_dir: tmpl.snapshot_dir,
        work_dir,
        overrides: SnapshotPathOverrides::from_original(
            &base_dir,
            &tmpl.original_task_vsock,
            sandbox_id,
        ),
        lease_mode: TemplateLeaseMode::Exclusive,
        snapshot_type: SnapshotType::Continuation,
        disk_images: tmpl.disk_images,
        storages: tmpl.storages,
        orphan_container_ids: tmpl.orphan_container_ids,
        memory_restore_mode,
        reflink_supported,
        warm_fork_params: None,
    };

    // Drop the Network handle accumulated from the original pod's prepare_network().
    // The CNI teardown for the original pod is handled separately; the IFF_PERSIST tap
    // survives.  Clearing network here ensures start_from_snapshot takes the
    // "same-node tap preserved" code path (self.network.is_none()) instead of
    // calling refresh_instance_identity() via the "rebuild-network" path.
    sandbox.network.take();

    // Clear the stale ttrpc client left from the original running sandbox.
    // init_client() inside start_from_snapshot only creates a new client when
    // self.client is None; if the old (broken) client stays, it skips reconnect
    // and all subsequent ttrpc calls (adopt_container etc.) fail with SendError.
    *sandbox.client.lock().await = None;

    // Reopen the IFF_PERSIST tap in the preserved netns (same-node restore).
    if !sandbox.data.netns.is_empty() {
        let config_json = src.snapshot_dir.join("config.json");
        if let Err(e) = sandbox.reopen_continuation_taps(&config_json).await {
            lease.fail().await;
            return Err(e);
        }
    }

    if let Err(e) = sandbox.start_from_snapshot(src).await {
        sandbox.clear_template_restore_state();
        lease.fail().await;
        return Err(e);
    }

    monitor(sandbox_mutex.clone());

    if !sandbox.sandbox_cgroups.cgroup_parent_path.is_empty() {
        if let Err(e) = sandbox.add_to_cgroup().await {
            sandbox.clear_template_restore_state();
            if let Err(re) = sandbox.vm.stop(true).await {
                warn!(
                    "service:sandbox {} rollback cgroup after resume: {}",
                    sandbox_id, re
                );
            }
            lease.fail().await;
            return Err(e);
        }
    }

    if let Err(e) = sandbox.dump().await {
        sandbox.clear_template_restore_state();
        if let Err(re) = sandbox.vm.stop(true).await {
            warn!(
                "service:sandbox {} rollback dump after resume: {}",
                sandbox_id, re
            );
        }
        lease.fail().await;
        return Err(e);
    }

    lease.complete().await;
    info!("service:sandbox {} resumed from pause snapshot", sandbox_id);
    Ok(())
}
