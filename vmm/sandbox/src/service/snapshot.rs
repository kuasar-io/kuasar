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

use std::collections::HashMap;

use anyhow::anyhow;
use containerd_sandbox::{error::Result, SandboxStatus};
use log::info;
use vmm_common::mount::bind_mount;

use super::Handle;
use crate::{
    sandbox::parse_warm_fork_container_names,
    template::{
        PooledTemplate, SnapshotContainerMeta, SnapshotType, TemplateKey, WorkloadIdentity,
    },
    vm::{
        DiskSnapshot, Snapshottable, VMFactory, WarmForkTarget, ANNOTATION_WARM_FORK_CONTAINERS,
        ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET, ANNOTATION_WARM_FORK_READINESS_SOCKET,
        ANNOTATION_WARM_FORK_READY_PROTOCOL, VIRTIO_BLK, VM, WARM_FORK_PROTOCOL_V1,
    },
};

/// Snapshot a running sandbox's VM and add the result to the template pool.
///
/// The sandbox's VM is paused, snapshotted, and immediately resumed so the
/// original container continues running.
///
/// `key` is the user-supplied pool key; pods with `kuasar.io/template-key=<key>`
/// will be matched to this template at start time.
pub async fn snapshot_from_sandbox<F>(
    handle: &Handle<F>,
    sandbox_id: &str,
    template_id: &str,
    key: &str,
    snapshot_type: SnapshotType,
    workload_identity: Option<WorkloadIdentity>,
) -> Result<PooledTemplate>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    let pool_opt = handle.pool.as_ref();
    if !matches!(snapshot_type, SnapshotType::Continuation) {
        pool_opt.ok_or_else(|| anyhow!("template pool not configured"))?;
    }
    let sandbox_mutex = handle
        .sandboxes
        .read()
        .await
        .get(sandbox_id)
        .ok_or_else(|| anyhow!("sandbox {} not found", sandbox_id))?
        .clone();

    let mut sandbox = sandbox_mutex.lock().await;
    if !matches!(sandbox.status, SandboxStatus::Running(_)) {
        return Err(anyhow!(
            "snapshot_from_sandbox requires sandbox {} to be running, current status: {:?}",
            sandbox_id,
            sandbox.status
        )
        .into());
    }

    // Both WarmFork and Continuation require virtio-blk: virtiofs shares container layers via a
    // vhost-user socket bound to a specific virtiofsd process, whose file descriptors
    // cannot be transferred across the snapshot/restore boundary.
    if sandbox.storage_policy.storage_backend != VIRTIO_BLK {
        return Err(anyhow!(
            "WarmFork/Continuation snapshots require container_storage_backend=virtio-blk; \
             current type '{}' cannot be snapshotted with active containers.",
            sandbox.storage_policy.storage_backend
        )
        .into());
    }

    // WarmFork-specific pre-snapshot checks: the process must already be blocking on the
    // readiness socket before vm.snapshot() pauses the VM.  Checking after snapshot would
    // allow a non-ready process to be frozen and later injected, producing a sandbox that
    // executes from an undefined state.
    let warmfork_check_result = if matches!(snapshot_type, SnapshotType::WarmFork) {
        let protocol_version = sandbox
            .data
            .config
            .as_ref()
            .and_then(|cfg| cfg.annotations.get(ANNOTATION_WARM_FORK_READY_PROTOCOL))
            .cloned()
            .unwrap_or_default();
        if protocol_version != WARM_FORK_PROTOCOL_V1 {
            return Err(anyhow!(
                "sandbox {}: cannot create WarmFork template — pod annotation '{}={}' is required; \
                 found: '{}'",
                sandbox_id,
                ANNOTATION_WARM_FORK_READY_PROTOCOL,
                WARM_FORK_PROTOCOL_V1,
                protocol_version,
            )
            .into());
        }
        let annotations = sandbox.data.config.as_ref().map(|c| &c.annotations);
        let pod_socket = annotations
            .and_then(|a| a.get(ANNOTATION_WARM_FORK_READINESS_SOCKET))
            .map(|s| s.as_str())
            .unwrap_or(ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET);

        // Build a name→container_id reverse map from the running containers' OCI specs.
        // Kubernetes sets "io.kubernetes.container.name" in the OCI spec annotations.
        let name_to_cid: HashMap<String, String> = sandbox
            .containers
            .iter()
            .filter_map(|(cid, container)| {
                container
                    .data
                    .spec
                    .as_ref()
                    .and_then(|s| s.annotations.get("io.kubernetes.container.name"))
                    .map(|name| (name.clone(), cid.clone()))
            })
            .collect();

        let (check_targets, resolved_warm_fork_targets): (
            Vec<vmm_common::api::sandbox::InjectTarget>,
            Vec<WarmForkTarget>,
        ) = if let Some(names) = annotations
            .map(parse_warm_fork_container_names)
            .transpose()
            .map_err(|e| {
                anyhow!(
                    "sandbox {}: invalid WarmFork target annotation: {}",
                    sandbox_id,
                    e
                )
            })?
            .flatten()
        {
            let mut check_targets = Vec::with_capacity(names.len());
            let mut wf_targets = Vec::with_capacity(names.len());
            for name in names {
                let socket = annotations
                    .and_then(|a| {
                        a.get(&format!(
                            "kuasar.io/container/{}/warm-fork-readiness-socket",
                            &name
                        ))
                    })
                    .cloned()
                    .unwrap_or_else(|| pod_socket.to_string());
                let container_id = match name_to_cid.get(&name) {
                    Some(id) => id.clone(),
                    None => {
                        return Err(anyhow!(
                            "sandbox {}: WarmFork container '{}' not found in running sandbox — \
                             cannot resolve to a CRI container ID; ensure the container has OCI \
                             spec annotation 'io.kubernetes.container.name'",
                            sandbox_id,
                            name
                        )
                        .into());
                    }
                };
                let mut it = vmm_common::api::sandbox::InjectTarget::new();
                it.container_id = container_id.clone();
                it.socket_path = socket.clone();
                check_targets.push(it);
                wf_targets.push(WarmForkTarget {
                    container_name: name,
                    container_id,
                    socket_path: socket,
                    env_overrides: Default::default(),
                    context: String::new(),
                });
            }
            (check_targets, wf_targets)
        } else {
            let container_id =
                match sandbox.containers.len() {
                    1 => sandbox.containers.keys().next().cloned().ok_or_else(|| {
                        anyhow!("sandbox {}: no running container found", sandbox_id)
                    })?,
                    0 => {
                        return Err(anyhow!(
                        "sandbox {}: cannot create WarmFork template in single-container mode: \
                         no running container found",
                        sandbox_id
                    )
                        .into());
                    }
                    n => {
                        return Err(anyhow!(
                            "sandbox {}: WarmFork '{}' annotation is required for pods with {} \
                         containers; single-container mode requires exactly one running container",
                            sandbox_id,
                            ANNOTATION_WARM_FORK_CONTAINERS,
                            n
                        )
                        .into());
                    }
                };
            let mut it = vmm_common::api::sandbox::InjectTarget::new();
            it.container_id = container_id.clone();
            it.socket_path = pod_socket.to_string();
            (
                vec![it],
                vec![WarmForkTarget {
                    container_name: String::new(),
                    container_id,
                    socket_path: pod_socket.to_string(),
                    env_overrides: Default::default(),
                    context: String::new(),
                }],
            )
        };

        sandbox
            .check_inject_socket(&check_targets)
            .await
            .map_err(|e| {
                anyhow!(
                    "sandbox {}: WarmFork ready-check failed before snapshot: {}",
                    sandbox_id,
                    e
                )
            })?;
        Some((protocol_version, resolved_warm_fork_targets))
    } else {
        None
    };

    let snapshot_dir = if matches!(snapshot_type, SnapshotType::Continuation) {
        let store = handle.continuation_store.as_ref().ok_or_else(|| {
            anyhow!("continuation store not configured (enable_continuation_restore=true required)")
        })?;
        let dir = store.entry_dir_by_key(key).join("snapshot");
        if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
            return Err(anyhow!(
                "continuation snapshot for key='{}' already exists at {}; \
                 remove it before re-snapshotting",
                key,
                dir.display()
            )
            .into());
        }
        dir
    } else {
        pool_opt
            .ok_or_else(|| anyhow!("template pool not configured"))?
            .store_dir
            .join("warmfork")
            .join(template_id)
            .join("snapshot")
    };
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    let source_id_generator = sandbox.id_generator;
    let storages = sandbox.storages.clone();

    let mut orphan_container_ids: Vec<String> = sandbox.containers.keys().cloned().collect();
    for id in &sandbox.restore.orphan_container_ids {
        if !orphan_container_ids.contains(id) {
            orphan_container_ids.push(id.clone());
        }
    }

    let snapshot_containers: Vec<SnapshotContainerMeta> = sandbox
        .storages
        .iter()
        .filter(|s| s.owned_by_runtime && s.device_id.is_some() && s.cleanup_path.is_some())
        .flat_map(|s| {
            let device_id = s.device_id.clone().unwrap_or_default();
            let lower_dirs = s.lower_dirs.clone().unwrap_or_default();
            let img_path = s.cleanup_path.clone().unwrap_or_default();
            let storage_id = s.id.clone();
            s.ref_container
                .keys()
                .map(|cid| SnapshotContainerMeta {
                    id: cid.clone(),
                    lower_dirs: lower_dirs.clone(),
                    storage_id: storage_id.clone(),
                    device_id: device_id.clone(),
                    img_path: img_path.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let disks: Vec<DiskSnapshot> = sandbox
        .storages
        .iter()
        .filter(|s| s.owned_by_runtime)
        .filter_map(|s| {
            let did = s.device_id.as_ref()?;
            let img_path = s.cleanup_path.as_ref()?;
            Some(DiskSnapshot {
                storage_id: s.id.clone(),
                device_id: did.clone(),
                img_path: img_path.clone(),
            })
        })
        .collect();

    let meta = sandbox
        .vm
        .snapshot(&snapshot_dir, &disks)
        .await
        .map_err(|e| anyhow!("snapshot sandbox {}: {}", sandbox_id, e))?;

    let tmpl_key = TemplateKey::user(key);
    let mut tmpl = PooledTemplate::new(
        template_id,
        tmpl_key,
        meta.snapshot_dir,
        handle.factory.image_path(),
        handle.factory.kernel_path(),
        handle.factory.vcpus(),
        handle.factory.memory_mb(),
        meta.original_task_vsock,
        meta.original_console_path,
    );

    let (ready_protocol_version, warm_fork_targets) = match warmfork_check_result {
        Some((pv, wft)) => (Some(pv), wft),
        None => (None, vec![]),
    };

    tmpl.snapshot_type = snapshot_type;
    tmpl.ready_protocol_version = ready_protocol_version;
    tmpl.warm_fork_targets = warm_fork_targets;
    tmpl.workload_identity = workload_identity;
    tmpl.id_generator = source_id_generator;
    tmpl.disk_images = meta.disk_images;
    tmpl.storages = storages;
    tmpl.orphan_container_ids = orphan_container_ids;
    tmpl.snapshot_containers = snapshot_containers;
    if matches!(tmpl.snapshot_type, SnapshotType::Continuation) {
        let store = handle
            .continuation_store
            .as_ref()
            .ok_or_else(|| anyhow!("continuation store not configured"))?;
        // Preserve the netns with a bind mount so it survives CNI teardown when the original
        // pod is stopped. The original CNI-managed path (e.g. /var/run/netns/cni-xxx) is
        // deleted during pod teardown; the bind mount at {entry_dir}/preserved_netns keeps
        // the namespace accessible until the Continuation snapshot is consumed or deleted.
        if !sandbox.data.netns.is_empty() {
            let entry_dir = store.entry_dir_by_key(&tmpl.key.key);
            tokio::fs::create_dir_all(&entry_dir)
                .await
                .map_err(|e| anyhow!("create continuation entry dir: {}", e))?;
            let preserved = entry_dir.join("preserved_netns");
            tokio::fs::write(&preserved, b"")
                .await
                .map_err(|e| anyhow!("create preserved_netns placeholder: {}", e))?;
            bind_mount(&sandbox.data.netns, preserved.to_str().unwrap_or(""), &[])
                .map_err(|e| anyhow!("bind mount netns for continuation snapshot: {}", e))?;
            tmpl.netns = preserved.to_string_lossy().into_owned();
        } else {
            tmpl.netns = String::new();
        }
        store.save(&tmpl).await?;
        info!(
            "template {}: saved to continuation store (sandbox {}, key={}, pod_uid={}, generation={})",
            template_id,
            sandbox_id,
            tmpl.key.key,
            tmpl.workload_identity.as_ref().map(|wi| wi.pod_uid.as_str()).unwrap_or("?"),
            tmpl.workload_identity.as_ref().map(|wi| wi.generation).unwrap_or(0),
        );
    } else {
        tmpl.netns = sandbox.data.netns.clone();
        let pool = pool_opt.ok_or_else(|| anyhow!("template pool not configured"))?;
        pool.add(tmpl.clone()).await?;
        info!(
            "template {}: created from sandbox {} (type={:?}, pool_depth={})",
            template_id,
            sandbox_id,
            tmpl.snapshot_type,
            pool.depth(&tmpl.key).await,
        );
    }
    Ok(tmpl)
}
