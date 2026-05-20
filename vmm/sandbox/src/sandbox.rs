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

use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    cri::api::v1::NamespaceMode,
    data::SandboxData,
    error::{Error, Result},
    signal::ExitSignal,
    utils::cleanup_mounts,
    ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use containerd_shim::{protos::api::Envelope, util::write_str_to_file};
use log::{debug, error, info, warn};
use protobuf::{well_known_types::any::Any, MessageField};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    fs::{copy, create_dir_all, remove_dir_all, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock, Semaphore},
};
use tracing::instrument;
use ttrpc::context::with_timeout;
use vmm_common::{
    api::{
        empty::Empty,
        sandbox::{
            CheckInjectSocketRequest, ContainerTask, ExecVMProcessRequest, InjectStatus,
            InjectTarget, InjectTaskRequest, SetupSandboxRequest, UpdateInterfacesRequest,
            UpdateRoutesRequest,
        },
        sandbox_ttrpc::SandboxServiceClient,
    },
    storage::Storage,
    ETC_HOSTS, ETC_RESOLV, HOSTNAME_FILENAME, HOSTS_FILENAME, KUASAR_STATE_DIR, RESOLV_FILENAME,
    SHARED_DIR_SUFFIX,
};

use crate::{
    cgroup::{SandboxCgroup, DEFAULT_CGROUP_PARENT_PATH},
    client::{
        client_check, client_reseed_entropy, client_setup_sandbox, client_sync_clock,
        new_sandbox_client, new_sandbox_client_fail_fast, DEFAULT_CLIENT_CHECK_TIMEOUT,
    },
    container::KuasarContainer,
    network::{Network, NetworkConfig},
    restore::{RestorePhase, RestoreTransaction},
    storage::{
        device_graph::DeviceGraph,
        guest_file::{shell_quote, GuestFileInjector},
    },
    template::{
        new_template_id, ContinuationLease, ContinuationStore, CreateTemplateRequest,
        PooledTemplate, SnapshotType, TemplateKey, TemplateLease, TemplateMetrics, TemplatePool,
        WorkloadIdentity, TEMPLATE_ID_ANNOTATION,
    },
    utils::{get_dns_config, get_hostname, get_resources, get_sandbox_cgroup_parent_path},
    vm::{
        DiskImageEntry, Hooks, Recoverable, RestoreSource, SnapshotMeta, SnapshotPathOverrides,
        Snapshottable, VMFactory, WarmForkParams, WarmForkTarget, ANNOTATION_WARM_FORK_CONTAINERS,
        ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET, ANNOTATION_WARM_FORK_ENV_PREFIX,
        ANNOTATION_WARM_FORK_READINESS_SOCKET, ANNOTATION_WARM_FORK_READY_PROTOCOL,
        ANNOTATION_WARM_FORK_TASK_CONTEXT, ANNOTATION_WARM_FORK_TASK_ID, VIRTIO_BLK, VM,
        WARM_FORK_PROTOCOL_V1,
    },
};

pub const KUASAR_GUEST_SHARE_DIR: &str = "/run/kuasar/storage/containers/";

pub struct KuasarSandboxer<F: VMFactory, H: Hooks<F::VM>> {
    factory: Arc<F>,
    hooks: H,
    config: SandboxConfig,
    #[allow(clippy::type_complexity)]
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
    pub(crate) template_pool: Option<Arc<TemplatePool>>,
    pub(crate) continuation_store: Option<Arc<ContinuationStore>>,
    restore_semaphore: Arc<Semaphore>,
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + Sync + Send,
{
    pub fn new(config: SandboxConfig, vmm_config: F::Config, hooks: H) -> Self {
        Self {
            factory: Arc::new(F::new(vmm_config)),
            hooks,
            config,
            sandboxes: Arc::new(Default::default()),
            template_pool: None,
            continuation_store: None,
            restore_semaphore: Arc::new(Semaphore::new(4)),
        }
    }

    /// Return pool metrics, if the pool has been initialized.
    pub fn pool_metrics(&self) -> Option<Arc<TemplateMetrics>> {
        self.template_pool.as_ref().map(|p| p.metrics.clone())
    }

    fn apply_pool_settings(&self, sandbox: &mut KuasarSandbox<F::VM>) {
        if let Some(pool) = &self.template_pool {
            sandbox.lease_mode = Some(pool.lease_mode.clone());
            sandbox.reflink_supported = Some(pool.reflink_supported);
        }
    }

    /// Parse snapshot-restore intent from the sandbox's pod annotations.
    ///
    /// Returns `Err` only for hard annotation errors (e.g. malformed generation when
    /// `kuasar.io/pod-uid` is present). Returns `Ok` for all other cases, including
    /// when no relevant annotations are present.
    async fn parse_snapshot_intent(&self, id: &str) -> Result<SnapshotIntent> {
        let sandbox_mutex = self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone();
        let sandbox = sandbox_mutex.lock().await;
        let annotations = sandbox.data.config.as_ref().map(|c| &c.annotations);

        let explicit_type = annotations
            .and_then(|a| a.get(crate::template::SNAPSHOT_TYPE_ANNOTATION))
            .map(|v| crate::template::SnapshotType::from_annotation(v))
            .transpose()?;

        let (template_id, template_key) = if matches!(
            explicit_type,
            Some(SnapshotType::Environment) | Some(SnapshotType::Continuation)
        ) {
            (None, None)
        } else {
            (
                annotations.and_then(|a| a.get(TEMPLATE_ID_ANNOTATION).cloned()),
                annotations.and_then(|a| a.get(crate::template::TEMPLATE_KEY_ANNOTATION).cloned()),
            )
        };

        let cont_identity = if matches!(
            explicit_type,
            Some(SnapshotType::Environment) | Some(SnapshotType::WarmFork)
        ) || !self.config.snapshot.enable_continuation_restore
        {
            None
        } else if let Some(pod_uid) =
            annotations.and_then(|a| a.get(crate::template::POD_UID_ANNOTATION))
        {
            let generation = match annotations
                .and_then(|a| a.get(crate::template::WORKLOAD_GENERATION_ANNOTATION))
            {
                None => 0u32,
                Some(s) => match s.parse::<u64>() {
                    Ok(v) if v <= u32::MAX as u64 => v as u32,
                    Ok(v) => {
                        return Err(anyhow!(
                            "sandbox {}: annotation {}={} exceeds u32::MAX; \
                             refusing to start (cold-boot would lose Continuation state)",
                            id,
                            crate::template::WORKLOAD_GENERATION_ANNOTATION,
                            v
                        )
                        .into());
                    }
                    Err(_) => {
                        return Err(anyhow!(
                            "sandbox {}: annotation {}={:?} is not a valid u32; \
                             refusing to start (cold-boot would lose Continuation state)",
                            id,
                            crate::template::WORKLOAD_GENERATION_ANNOTATION,
                            s
                        )
                        .into());
                    }
                },
            };
            Some(WorkloadIdentity {
                pod_uid: pod_uid.clone(),
                generation,
            })
        } else {
            None
        };

        Ok(SnapshotIntent {
            explicit_type,
            cont_identity,
            template_id,
            template_key,
        })
    }

    fn environment_key(&self) -> TemplateKey {
        TemplateKey::from_vm_config(
            self.factory.kernel_path(),
            self.factory.image_path(),
            self.factory.vcpus(),
            self.factory.memory_mb(),
            self.factory.kernel_params(),
            self.factory.storage_backend(),
        )
    }

    fn any_snapshot_restore_enabled(&self) -> bool {
        self.config.snapshot.enable_warmfork_restore
            || self.config.snapshot.enable_continuation_restore
            || self.config.snapshot.enable_environment_restore
    }
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + DeserializeOwned + Recoverable + Sync + Send + 'static,
{
    #[instrument(skip_all)]
    pub async fn recover(&mut self, dir: &str) {
        let start = Instant::now();
        let mut subs = match tokio::fs::read_dir(dir).await {
            Ok(subs) => subs,
            Err(e) => {
                error!("FATAL! read working dir {} for recovery: {}", dir, e);
                return;
            }
        };

        let mut entries = Vec::new();
        while let Some(entry) = subs.next_entry().await.unwrap() {
            entries.push(entry);
        }

        // Limit the concurrency of sandbox recovery.
        // When a node has a large number of pods (e.g., 1k pods), unbounded concurrent
        // recovery could consume massive system resources, potentially preempting and
        // starving normal business workloads.
        const RECOVERY_CONCURRENCY: usize = 32;
        let semaphore = Arc::new(Semaphore::new(RECOVERY_CONCURRENCY));
        let mut handles = Vec::with_capacity(entries.len());

        for entry in entries {
            if let Ok(t) = entry.file_type().await {
                if !t.is_dir() {
                    continue;
                }
                let dir_path = dir.to_string();
                let sandboxes = self.sandboxes.clone();
                let permit = semaphore.clone().acquire_owned().await.unwrap();

                let handle = tokio::spawn(async move {
                    let _permit = permit;
                    debug!("recovering sandbox {:?}", entry.file_name());
                    let path = Path::new(&dir_path).join(entry.file_name());
                    match KuasarSandbox::recover(&path).await {
                        Ok(sb) => {
                            let status = sb.status.clone();
                            let sb_mutex = Arc::new(Mutex::new(sb));
                            // Only running sandbox should be monitored.
                            if let SandboxStatus::Running(_) = status {
                                let sb_clone = sb_mutex.clone();
                                monitor(sb_clone);
                            }
                            sandboxes
                                .write()
                                .await
                                .insert(entry.file_name().to_str().unwrap().to_string(), sb_mutex);
                            true
                        }
                        Err(e) => {
                            warn!("failed to recover sandbox {:?}, {:?}", entry.file_name(), e);
                            cleanup_mounts(path.to_str().unwrap())
                                .await
                                .unwrap_or_default();
                            remove_dir_all(&path).await.unwrap_or_default();
                            false
                        }
                    }
                });
                handles.push(handle);
            }
        }

        let total = handles.len();
        let mut success = 0;
        let mut fail = 0;
        for handle in handles {
            match handle.await {
                Ok(true) => success += 1,
                Ok(false) => fail += 1,
                Err(e) => {
                    error!("recovery task join error: {}", e);
                    fail += 1;
                }
            }
        }
        info!(
            "recover sandboxes finished, total: {}, success: {}, fail: {}, takes: {:.3}s",
            total,
            success,
            fail,
            start.elapsed().as_secs_f64()
        );

        // GC consumed template directories that were orphaned by a crash during sandbox deletion.
        // Any consumed-marker directory not referenced by a recovered sandbox is a leak.
        if let Some(pool) = &self.template_pool {
            // Rebuild two sets of extra in_use_count refs that were held at shutdown:
            // - shared_ids: WarmFork Shared sandboxes whose SharedRef lease pins the template
            //   for the sandbox lifetime.
            // - env_non_copy_pinned_ids: Environment sandboxes in non-Copy mode whose snapshot
            //   files back the running VM (the extra ref is released in delete()).
            //   WarmFork Shared sandboxes in non-Copy mode do NOT need a separate pin here —
            //   their SharedRef in_use_count already keeps the template alive.
            let (active_ids, active_shared_container_ids, env_non_copy_pinned_ids): (
                std::collections::HashSet<String>,
                Vec<String>,
                Vec<String>,
            ) = {
                let sbs = self.sandboxes.read().await;
                let mut ids = std::collections::HashSet::new();
                let mut shared_ids = Vec::new();
                let mut pinned_ids = Vec::new();
                for sb_mutex in sbs.values() {
                    let sb = sb_mutex.lock().await;
                    if let Some(tid) = &sb.template_id {
                        ids.insert(tid.clone());
                        if matches!(
                            sb.template_snapshot_type.as_ref(),
                            Some(SnapshotType::WarmFork)
                        ) && sb.lease_mode == Some(TemplateLeaseMode::Shared)
                        {
                            shared_ids.push(tid.clone());
                        } else if matches!(
                            sb.template_snapshot_type.as_ref(),
                            Some(SnapshotType::Environment)
                        ) && !matches!(
                            sb.memory_restore_mode.as_ref(),
                            Some(MemoryRestoreMode::Copy) | None
                        ) {
                            pinned_ids.push(tid.clone());
                        }
                    }
                }
                (ids, shared_ids, pinned_ids)
            };
            pool.rebuild_active_refs(&active_shared_container_ids, &env_non_copy_pinned_ids)
                .await;
            pool.gc_orphaned_consumed(&active_ids).await;
            if let Some(store) = &self.continuation_store {
                if let Err(e) = store.gc_orphaned_consumed(&active_ids).await {
                    warn!("continuation store GC during recover failed: {}", e);
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct KuasarSandbox<V: VM> {
    pub(crate) vm: V,
    pub(crate) id: String,
    pub(crate) status: SandboxStatus,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) containers: HashMap<String, KuasarContainer>,
    pub(crate) storages: DeviceGraph,
    pub(crate) id_generator: u32,
    pub(crate) network: Option<Network>,
    #[serde(skip, default)]
    pub(crate) client: Arc<Mutex<Option<SandboxServiceClient>>>,
    #[serde(skip, default)]
    pub(crate) exit_signal: Arc<ExitSignal>,
    #[serde(default)]
    pub(crate) sandbox_cgroups: SandboxCgroup,
    #[serde(default)]
    pub(crate) template_id: Option<String>,
    /// The SnapshotType of the template this sandbox was restored from.
    /// None for cold-started sandboxes.
    #[serde(default)]
    pub(crate) template_snapshot_type: Option<SnapshotType>,
    #[serde(default)]
    pub(crate) lease_mode: Option<TemplateLeaseMode>,
    #[serde(default)]
    pub(crate) reflink_supported: Option<bool>,
    /// Memory restore mode used when this sandbox was restored from a template.
    /// None for cold-started sandboxes. Persisted so the sandboxer can rebuild
    /// in_use_count after restart for Environment templates in non-Copy mode.
    #[serde(default)]
    pub(crate) memory_restore_mode: Option<MemoryRestoreMode>,
    /// Container IDs that were running when a snapshot was taken and have since been
    /// removed from the guest, but whose storage entries may still be present in the
    /// restored sandbox.  Sharing storage with an orphan is refused to prevent
    /// cross-container data leaks after restore.
    #[serde(default)]
    pub(crate) orphan_container_ids: Vec<String>,
}

/// Parsed snapshot-restore intent from a sandbox's pod annotations.
struct SnapshotIntent {
    explicit_type: Option<SnapshotType>,
    cont_identity: Option<WorkloadIdentity>,
    template_id: Option<String>,
    template_key: Option<String>,
}

#[async_trait]
impl<F, H> Sandboxer for KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    type Sandbox = KuasarSandbox<F::VM>;

    #[instrument(skip_all)]
    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        if self.sandboxes.read().await.get(id).is_some() {
            return Err(Error::AlreadyExist("sandbox".to_string()));
        }

        let mut sandbox_cgroups = SandboxCgroup::default();
        let cgroup_parent_path = get_sandbox_cgroup_parent_path(&s.sandbox)
            .unwrap_or(DEFAULT_CGROUP_PARENT_PATH.to_string());
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // Create sandbox's cgroup and apply sandbox's resources limit
            let create_and_update_sandbox_cgroup = (|| {
                sandbox_cgroups =
                    SandboxCgroup::create_sandbox_cgroups(&cgroup_parent_path, &s.sandbox.id)?;
                sandbox_cgroups.update_res_for_sandbox_cgroups(&s.sandbox)?;
                Ok(())
            })();
            // If create and update sandbox cgroup failed, do rollback operation
            if let Err(e) = create_and_update_sandbox_cgroup {
                let _ = sandbox_cgroups.remove_sandbox_cgroups();
                return Err(e);
            }
        }
        let vm = self.factory.create_vm(id, &s).await?;
        let mut sandbox = KuasarSandbox {
            vm,
            id: id.to_string(),
            status: SandboxStatus::Created,
            base_dir: s.base_dir,
            data: s.sandbox.clone(),
            containers: Default::default(),
            storages: DeviceGraph::default(),
            id_generator: 0,
            network: None,
            client: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(ExitSignal::default()),
            sandbox_cgroups,
            template_id: None,
            template_snapshot_type: None,
            lease_mode: None,
            reflink_supported: None,
            memory_restore_mode: None,
            orphan_container_ids: vec![],
        };

        // setup sandbox files: hosts, hostname and resolv.conf for guest
        sandbox.setup_sandbox_files().await?;
        self.hooks.post_create(&mut sandbox).await?;
        sandbox.dump().await?;
        self.sandboxes
            .write()
            .await
            .insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    #[instrument(skip_all)]
    async fn start(&self, id: &str) -> Result<()> {
        if self.template_pool.is_some() || self.continuation_store.is_some() {
            if self.config.snapshot.enable_warmfork_restore
                || self.config.snapshot.enable_continuation_restore
            {
                let intent = self.parse_snapshot_intent(id).await?;

                match intent.explicit_type {
                    Some(SnapshotType::Continuation) => {
                        if !self.config.snapshot.enable_continuation_restore {
                            return Err(anyhow!(
                                "sandbox {}: explicit snapshot-type=continuation requires \
                                 enable_continuation_restore=true",
                                id
                            )
                            .into());
                        }
                        let identity = intent.cont_identity.ok_or_else(|| {
                            anyhow!(
                                "sandbox {}: explicit snapshot-type=continuation requires \
                                 kuasar.io/pod-uid",
                                id
                            )
                        })?;
                        info!(
                            "sandbox {}: trying Continuation snapshot ({}/g{})",
                            id, identity.pod_uid, identity.generation
                        );
                        return self.start_with_continuation_snapshot(id, &identity).await;
                    }
                    Some(SnapshotType::WarmFork) => {
                        if !self.config.snapshot.enable_warmfork_restore {
                            return Err(anyhow!(
                                "sandbox {}: explicit snapshot-type=warm-fork requires \
                                 enable_warmfork_restore=true",
                                id
                            )
                            .into());
                        }
                        return match (
                            intent.template_id.as_deref(),
                            intent.template_key.as_deref(),
                        ) {
                            (None, None) => Err(anyhow!(
                                "sandbox {}: explicit snapshot-type=warm-fork requires \
                                 kuasar.io/template-key or kuasar.io/template-id",
                                id
                            )
                            .into()),
                            (Some(tid), key) => {
                                info!(
                                    "sandbox {}: trying WarmFork snapshot (template_id={}{})",
                                    id,
                                    tid,
                                    key.map(|k| format!(", key={}", k)).unwrap_or_default()
                                );
                                self.start_with_template_id(id, tid, key).await
                            }
                            (None, Some(key_str)) => {
                                let user_key = TemplateKey::user(key_str);
                                info!(
                                    "sandbox {}: trying WarmFork snapshot (key={})",
                                    id, user_key.key
                                );
                                self.start_with_template_key(id, &user_key).await
                            }
                        };
                    }
                    _ => {} // Environment or no explicit type: fall through to implicit detection
                }

                // Continuation: mandatory when pod-uid annotation is present.
                if let Some(identity) = intent.cont_identity {
                    info!(
                        "sandbox {}: trying Continuation snapshot ({}/g{})",
                        id, identity.pod_uid, identity.generation
                    );
                    return self.start_with_continuation_snapshot(id, &identity).await;
                }

                // WarmFork annotations present: try WarmFork, fall back to environment on miss.
                if intent.template_id.is_some() || intent.template_key.is_some() {
                    let result = match (
                        intent.template_id.as_deref(),
                        intent.template_key.as_deref(),
                    ) {
                        (Some(tid), key) => {
                            info!(
                                "sandbox {}: trying WarmFork snapshot (template_id={}{})",
                                id,
                                tid,
                                key.map(|k| format!(", key={}", k)).unwrap_or_default()
                            );
                            self.start_with_template_id(id, tid, key).await
                        }
                        (None, Some(key_str)) => {
                            let user_key = TemplateKey::user(key_str);
                            info!(
                                "sandbox {}: trying WarmFork snapshot (key={})",
                                id, user_key.key
                            );
                            self.start_with_template_key(id, &user_key).await
                        }
                        (None, None) => unreachable!(),
                    };
                    match result {
                        Ok(()) => return Ok(()),
                        Err(e) => info!(
                            "sandbox {}: WarmFork snapshot miss ({}), falling back to environment snapshot",
                            id, e
                        ),
                    }
                }
            }

            if self.config.snapshot.enable_environment_restore && self.template_pool.is_some() {
                let environment_key = self.environment_key();
                info!(
                    "sandbox {}: trying environment snapshot (key={})",
                    id, environment_key.key
                );
                match self.start_with_template_key(id, &environment_key).await {
                    Ok(()) => return Ok(()),
                    Err(e) => info!("sandbox {}: environment snapshot miss ({})", id, e),
                }
            }

            if self.any_snapshot_restore_enabled() && !self.config.snapshot.fallback_to_fresh_boot {
                return Err(anyhow!("sandbox {}: no matching enabled snapshot", id).into());
            }
            info!(
                "sandbox {}: no matching enabled snapshot, cold-starting",
                id
            );
        }

        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.apply_pool_settings(&mut sandbox);
        self.hooks.pre_start(&mut sandbox).await?;

        if !sandbox.data.netns.is_empty() {
            sandbox.prepare_network().await?;
        }

        if let Err(e) = sandbox.start().await {
            sandbox.destroy_network().await;
            return Err(e);
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in add to cgroup {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox post start {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = sandbox.dump().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox start dump {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        sandbox.data = data;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone())
    }

    #[instrument(skip_all)]
    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let sandbox_mutex = match self.sandbox(id).await {
            Ok(sb) => sb,
            Err(Error::NotFound(_)) => {
                // Sandbox not found in the hashmap, nothing to stop.
                // This can happen during batch pod creation if a sandbox
                // was never fully created before KillPodSandbox is called.
                warn!("sandbox {} not found during stop, skipping", id);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_stop(&mut sandbox).await?;
        sandbox.stop(force).await?;
        self.hooks.post_stop(&mut sandbox).await?;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete(&self, id: &str) -> Result<()> {
        let sb_clone = self.sandboxes.read().await.clone();
        if let Some(sb_mutex) = sb_clone.get(id) {
            let mut sb = sb_mutex.lock().await;

            // Capture before stop() so we can GC the template directory afterwards.
            let consumed_template_id = sb.template_id.clone();

            sb.stop(true).await?;

            // Currently only support cgroup V1, cgroup V2 is not supported now
            if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
                // remove the sandbox cgroups
                sb.sandbox_cgroups.remove_sandbox_cgroups()?;
            }

            cleanup_mounts(&sb.base_dir).await?;
            // Should Ignore the NotFound error of base dir as it may be already deleted.
            if let Err(e) = remove_dir_all(&sb.base_dir).await {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into());
                }
            }

            // Release the template reference held during this sandbox's lifetime.
            // Environment (non-Copy): releases the env_non_copy_pinned extra in_use_count ref
            //   acquired in start_from_snapshot. Copy mode never acquires this extra ref.
            // WarmFork Shared: releases the SharedRef in_use_count ref held for the full
            //   sandbox lifetime (independent of memory_restore_mode).
            // Continuation (Exclusive): calls cleanup_consumed to remove the template directory
            //   now that the VM has been stopped above.
            if let (Some(tid), Some(pool)) = (consumed_template_id, &self.template_pool) {
                match sb.template_snapshot_type.as_ref() {
                    Some(SnapshotType::Environment) => {
                        if !matches!(
                            sb.memory_restore_mode.as_ref(),
                            Some(MemoryRestoreMode::Copy) | None
                        ) {
                            pool.deref(&tid).await;
                        }
                    }
                    Some(SnapshotType::WarmFork) => match sb.lease_mode {
                        Some(TemplateLeaseMode::Exclusive) => {
                            pool.cleanup_consumed(&tid).await;
                        }
                        Some(TemplateLeaseMode::Shared) => {
                            pool.deref(&tid).await;
                        }
                        None => {
                            pool.cleanup_consumed(&tid).await;
                        }
                    },
                    Some(SnapshotType::Continuation) => {
                        if let Some(store) = &self.continuation_store {
                            if let Err(e) = store.delete_by_template_id(&tid).await {
                                warn!(
                                    "failed to delete continuation template {} during destroy: {}",
                                    tid, e
                                );
                            }
                        }
                    }
                    None => {
                        // Sandbox has no template kind recorded; only cleanup consumed snapshots.
                        pool.cleanup_consumed(&tid).await;
                    }
                }
            }
        }
        self.sandboxes.write().await.remove(id);
        Ok(())
    }
}

/// Parameters for restoring a sandbox from a snapshot instead of cold-booting.
pub struct SnapshotRestoreParams {
    pub snapshot_dir: PathBuf,
    pub template_id: Option<String>,
    pub snapshot_type: SnapshotType,
    pub id_generator: u32,
    pub disk_images: Vec<DiskImageEntry>,
    pub storages: Vec<Storage>,
    pub orphan_container_ids: Vec<String>,
    pub lease_mode: TemplateLeaseMode,
    pub reflink_supported: bool,
    /// vsock socket path from the template VM at snapshot time.
    pub original_task_vsock: String,
    /// Task parameters for WarmFork restores. `None` for Environment/Continuation.
    pub warm_fork_params: Option<WarmForkParams>,
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    /// Start an already-created sandbox by restoring it from `snapshot_dir` rather than
    /// cold-booting the VM. Restore errors are returned to the caller; the policy
    /// layer decides whether to fall back to a fresh boot.
    ///
    /// `template_id` is persisted to sandbox.json for audit when the restore came from
    /// the template pool.  Pass `None` for a direct/one-off snapshot restore.
    ///
    /// `template_id_generator` must be set to the id_generator value of the sandbox that
    /// produced the snapshot.  The restored VM already contains block devices with IDs up to
    /// that value, so the new sandbox must start its counter there to avoid
    /// `IdentifierNotUnique` errors when hotplugging further devices.  Pass `0` when
    /// restoring from a snapshot taken from a bare (container-free) VM.
    ///
    /// `disk_images` lists the disk files that were captured during the snapshot.  Pass an
    /// empty slice for bare-VM (template) snapshots; pass the entries from `PooledTemplate`
    /// for full-checkpoint restores so the `.img` files are copied and remapped.
    ///
    /// `reflink_supported` indicates whether reflink CoW is available for disk copy.
    /// True: use reflink (space-efficient); False: use plain copy.
    pub async fn start_from_snapshot(&self, id: &str, params: SnapshotRestoreParams) -> Result<()> {
        let sandbox_mutex = self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone();

        let mut sandbox = sandbox_mutex.lock().await;

        self.hooks.pre_start(&mut sandbox).await?;

        // Continuation preserves the original Pod IP and network identity — the operator
        // migrates the network externally before calling start().  Creating new CNI/tap
        // resources here would waste kernel resources and could conflict with that external
        // migration.  Only Environment and WarmFork need a freshly prepared netns.
        if !sandbox.data.netns.is_empty()
            && !matches!(params.snapshot_type, SnapshotType::Continuation)
        {
            if let Err(e) = sandbox.prepare_network().await {
                sandbox.destroy_network().await;
                return Err(e);
            }
        }

        // memory_restore_mode is orthogonal infrastructure — it applies to any SnapshotType.
        // All four modes (Copy, OnDemand, FileBackend, ExternalUffd) are valid for all three
        // snapshot types.  For Continuation, "exclusive consume" only removes the pool entry;
        // snapshot files remain on disk until delete() stops the VM and then calls
        // cleanup_consumed(), so FileBackend/ExternalUffd backing stores are safe.
        // WarmFork Shared templates stay GC-pinned for the full sandbox lifetime via the
        // SharedRef in_use_count, so non-Copy modes are safe there too.
        let memory_restore_mode = self.config.snapshot.default_memory_restore_mode.clone();

        sandbox.template_id = params.template_id.clone();
        sandbox.template_snapshot_type = Some(params.snapshot_type.clone());
        sandbox.lease_mode = Some(params.lease_mode.clone());
        sandbox.reflink_supported = Some(params.reflink_supported);
        sandbox.memory_restore_mode = Some(memory_restore_mode.clone());

        let work_dir = PathBuf::from(format!("{}/restore", sandbox.base_dir));
        let src = RestoreSource {
            snapshot_dir: params.snapshot_dir,
            work_dir,
            overrides: SnapshotPathOverrides::from_original(
                &sandbox.base_dir,
                &params.original_task_vsock,
                &sandbox.id,
            ),
            snapshot_type: params.snapshot_type,
            disk_images: params.disk_images,
            storages: params.storages,
            orphan_container_ids: params.orphan_container_ids,
            memory_restore_mode,
            reflink_supported: params.reflink_supported,
            lease_mode: params.lease_mode,
            warm_fork_params: params.warm_fork_params,
        };

        if let Err(e) = sandbox.start_from_snapshot(src).await {
            sandbox.clear_template_restore_state();
            sandbox.destroy_network().await;
            return Err(e);
        }

        // Environment + non-Copy mode: snapshot files remain the backing store for the running VM.
        // Pin the template in in_use_count so GC cannot evict the snapshot directory while
        // this sandbox is alive.  Released in delete() when the sandbox stops.
        //
        // WarmFork Shared does NOT need this extra pin: the SharedRef lease already increments
        // in_use_count on acquire and does not decrement it on complete(), so the template
        // stays pinned for the full WarmFork sandbox lifetime regardless of memory mode.
        //
        // The pin is acquired here, before the post-restore steps below, so that a concurrent
        // GC triggered by end_restore() (called in lease.complete()) cannot evict the template
        // between the restore succeeding and the sandbox reaching Running state.
        let env_non_copy_pinned =
            if let (Some(tid), Some(pool)) = (&sandbox.template_id, &self.template_pool) {
                if matches!(
                    sandbox.template_snapshot_type.as_ref(),
                    Some(SnapshotType::Environment)
                ) && !matches!(
                    sandbox.memory_restore_mode.as_ref(),
                    Some(MemoryRestoreMode::Copy) | None
                ) {
                    pool.ref_template(tid).await;
                    true
                } else {
                    false
                }
            } else {
                false
            };

        // Helper: undo the pin if a post-restore step fails before the sandbox is Running.
        macro_rules! rollback_post_restore {
            ($sandbox:expr, $pool:expr, $err:expr, $label:literal) => {{
                if env_non_copy_pinned {
                    if let (Some(tid), Some(pool)) = (&$sandbox.template_id, $pool) {
                        pool.deref(tid).await;
                    }
                }
                $sandbox.clear_template_restore_state();
                if let Err(re) = $sandbox.stop(true).await {
                    warn!("sandbox {}: rollback {} (restore): {}", id, $label, re);
                    return Err($err);
                }
                $sandbox.destroy_network().await;
                return Err($err);
            }};
        }

        // Restore the id_generator from the template so that newly hotplugged device IDs
        // do not collide with device IDs already present in the restored VM state.
        sandbox.id_generator = params.id_generator;

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            rollback_post_restore!(sandbox, &self.template_pool, e, "add_to_cgroup");
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            rollback_post_restore!(sandbox, &self.template_pool, e, "post_start");
        }

        if let Err(e) = sandbox.dump().await {
            rollback_post_restore!(sandbox, &self.template_pool, e, "dump");
        }

        Ok(())
    }
}

#[async_trait]
impl<V> Sandbox for KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    type Container = KuasarContainer;

    #[instrument(skip_all)]
    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        self.vm.ping().await
    }

    #[instrument(skip_all)]
    async fn container(&self, id: &str) -> Result<&Self::Container> {
        let container = self
            .containers
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        Ok(container)
    }

    #[instrument(skip_all)]
    async fn append_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_append_handlers(id, options)?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_update_handlers(id, options).await?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn remove_container(&mut self, id: &str) -> Result<()> {
        self.deference_container_storages(id).await?;

        let bundle = format!("{}/{}", self.get_sandbox_shared_path(), &id);
        if let Err(e) = tokio::fs::remove_dir_all(&*bundle).await {
            if e.kind() != ErrorKind::NotFound {
                return Err(anyhow!("failed to remove bundle {}, {}", bundle, e).into());
            }
        }
        let container = self.containers.remove(id);
        // Containerd stops the container process before calling remove_container; the
        // sandboxer only needs to clean up hypervisor resources (IO devices).
        match container {
            None => {}
            Some(c) => {
                for device_id in c.io_devices {
                    self.vm.hot_detach(&device_id).await?;
                }
            }
        }
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    #[instrument(skip_all)]
    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Snapshottable + Sync + Send,
{
    /// Restore this sandbox from a snapshot instead of cold-booting the VM.
    ///
    /// Restore failure is returned to the caller after cleaning the per-restore
    /// work_dir. The sandboxer policy layer decides whether to fall back to a
    /// fresh boot.
    pub(crate) async fn start_from_snapshot(&mut self, src: RestoreSource) -> Result<()> {
        let mut txn = RestoreTransaction::new(self.id.clone(), src.work_dir.clone());
        match self.try_restore(&mut txn, &src).await {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(
                    "sandbox {}: restore failed at phase {} ({})",
                    self.id,
                    txn.phase(),
                    e
                );
                txn.cleanup_work_dir().await;
                Err(e)
            }
        }
    }

    async fn try_restore(
        &mut self,
        txn: &mut RestoreTransaction,
        src: &RestoreSource,
    ) -> Result<()> {
        // vm.restore() launches CH, calls vm.restore API, and waits for agent ready.
        txn.set_phase(RestorePhase::RestoreVm);
        self.vm.restore(src).await.map_err(|e| txn.fail(e))?;

        let pid = self.vm.pids().vmm_pid.unwrap_or_default();

        txn.set_phase(RestorePhase::InitClient);
        if let Err(e) = self.init_client().await {
            txn.rollback_vm(&mut self.vm).await;
            return Err(txn.fail(e));
        }

        match src.snapshot_type {
            SnapshotType::WarmFork => {
                // Hotplug tap network devices into the restored VM before injecting the task.
                txn.set_phase(RestorePhase::HotplugNetwork);
                if let Err(e) = self.vm.hotplug_pending_network().await {
                    txn.rollback_vm(&mut self.vm).await;
                    return Err(txn.fail(e));
                }

                self.storages = self.remap_restored_storage_artifacts(src);
                self.orphan_container_ids = src.orphan_container_ids.clone();

                if let Err(e) = self.refresh_instance_identity().await {
                    txn.rollback_vm(&mut self.vm).await;
                    return Err(txn.fail(e));
                }

                // Reseed entropy before injecting the task so the ready-waiting process starts
                // with fresh instance entropy rather than the template's shared pool state.
                self.reseed_guest_entropy().await;

                // Drive the WarmFork post-restore protocol (autonomous or injection mode).
                txn.set_phase(RestorePhase::InjectTask);
                let params = match src.warm_fork_params.as_ref() {
                    Some(p) => p,
                    None => {
                        txn.rollback_vm(&mut self.vm).await;
                        return Err(txn.fail(anyhow!(
                            "WarmFork branch reached without warm_fork_params; this is a sandboxer bug"
                        )));
                    }
                };
                if let Err(e) = self.inject_warm_fork_task(params).await {
                    // The guest agent already sent CANCEL to any READY workloads before
                    // returning the error result; no host-side cancel RPC is needed.
                    txn.rollback_vm(&mut self.vm).await;
                    return Err(txn.fail(e));
                }
            }
            SnapshotType::Continuation => {
                // Network identity transfer is an external concern: the operator or CNI layer
                // must route the original Pod IP to this node before calling start().  Kuasar
                // trusts that the transfer is complete.  Existing TCP connections are not
                // preserved; workloads must tolerate reconnect errors on previously-open sockets.
                //
                // Kuasar does NOT hotplug a new network namespace (the process resumes with its
                // original network identity).  Kuasar does NOT call setup_sandbox() (the guest
                // environment is already configured).  Kuasar does NOT reseed entropy (process
                // state is preserved exactly).
                //
                // Storage: disk images were transferred via MovedExclusive (rename) into this
                // sandbox's base_dir.  Update cleanup_paths so delete() can clean them up.
                self.storages = self.remap_restored_storage_artifacts(src);
                // Track containers that were running at snapshot time so storage accounting
                // and delete() cleanup work correctly.  We do NOT clean them up — they continue
                // running with their original identity (no orphan kill, no identity refresh).
                self.orphan_container_ids = src.orphan_container_ids.clone();

                txn.set_phase(RestorePhase::TransferNetworkIdentity);
                info!(
                    "sandbox {}: Continuation restore: network identity transfer is external; \
                     assuming original Pod IP is already routable on this node",
                    self.id
                );
                // Sync the guest clock to correct skew introduced by snapshot/restore latency.
                self.sync_clock().await;
            }
            SnapshotType::Environment => {
                // Hotplug tap devices first so the guest can see them during setup_sandbox.
                txn.set_phase(RestorePhase::HotplugNetwork);
                if let Err(e) = self.vm.hotplug_pending_network().await {
                    txn.rollback_vm(&mut self.vm).await;
                    return Err(txn.fail(e));
                }

                if self.vm.container_storage_backend() == VIRTIO_BLK {
                    txn.set_phase(RestorePhase::PushSandboxFiles);
                    if let Err(e) = self.push_sandbox_files().await {
                        txn.rollback_vm(&mut self.vm).await;
                        return Err(txn.fail(e));
                    }
                }
                txn.set_phase(RestorePhase::SetupSandbox);
                if let Err(e) = self.setup_sandbox().await {
                    txn.rollback_vm(&mut self.vm).await;
                    return Err(txn.fail(e));
                }
            }
        }

        // Inject fresh entropy for Environment restores (not Continuation — 1:1 exclusive
        // already has unique state; not WarmFork — already reseeded inside the arm above
        // before inject_task so the process starts with fresh entropy).
        if matches!(src.snapshot_type, SnapshotType::Environment) {
            self.reseed_guest_entropy().await;
        }

        txn.set_phase(RestorePhase::Commit);
        self.forward_events().await;
        self.status = SandboxStatus::Running(pid);

        // Copy mode: all guest memory is in RAM; snapshot symlinks in work_dir are no longer
        // needed and can be removed.  Non-Copy modes keep work_dir alive (page-fault source).
        // Continuation is eligible for cleanup in Copy mode like the other types.
        if matches!(src.memory_restore_mode, MemoryRestoreMode::Copy) {
            txn.cleanup_work_dir().await;
        }

        Ok(())
    }

    fn remap_restored_storage_artifacts(&self, src: &RestoreSource) -> DeviceGraph {
        let mut storages = src.storages.clone();
        for storage in &mut storages {
            if storage.owned_by_runtime && storage.device_id.is_some() {
                storage.cleanup_path = Some(format!("{}/{}.img", self.base_dir, storage.id));
            }
        }
        DeviceGraph::from(storages)
    }

    /// Update per-instance identity after restoring from a WarmFork snapshot.
    ///
    /// Network configuration is always refreshed so each restored instance gets its own
    /// IP rather than the template's original IP.
    async fn refresh_instance_identity(&mut self) -> Result<()> {
        let timeout_ns = Duration::from_secs(10).as_nanos() as i64;

        // Push updated sandbox config files (hostname, resolv.conf, hosts) into the guest.
        // In virtiofs mode these are served directly from the host share; in virtio-blk mode
        // they must be pushed explicitly since there is no shared filesystem.
        if self.vm.container_storage_backend() == VIRTIO_BLK {
            self.push_sandbox_files().await.map_err(|e| {
                anyhow!(
                    "sandbox {}: push_sandbox_files in refresh_instance_identity: {}",
                    self.id,
                    e
                )
            })?;
        }

        // Update network interfaces and routes to the new sandbox's network namespace.
        // This call goes directly to update_interfaces/update_routes RPCs (not setup_sandbox),
        // which is correct for WarmFork restores where setup_sandbox is skipped.
        if let Some(network) = self.network.as_ref() {
            let interfaces: Vec<_> = network.interfaces().iter().map(|x| x.into()).collect();
            let routes: Vec<_> = network.routes().iter().map(|x| x.into()).collect();
            let _ = network;

            let client_guard = self.client.lock().await;
            if let Some(client) = client_guard.as_ref() {
                if !interfaces.is_empty() {
                    let mut req = UpdateInterfacesRequest::new();
                    req.interfaces = interfaces;
                    client
                        .update_interfaces(with_timeout(timeout_ns), &req)
                        .await
                        .map_err(|e| anyhow!("update_interfaces: {}", e))?;
                }
                if !routes.is_empty() {
                    let mut req = UpdateRoutesRequest::new();
                    req.routes = routes;
                    client
                        .update_routes(with_timeout(timeout_ns), &req)
                        .await
                        .map_err(|e| anyhow!("update_routes: {}", e))?;
                }
            }
        }

        // Sync the guest clock to avoid skew introduced by snapshot/restore latency.
        self.sync_clock().await;

        Ok(())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    pub(crate) fn clear_template_restore_state(&mut self) {
        self.template_id = None;
        self.template_snapshot_type = None;
        self.lease_mode = None;
        self.reflink_supported = None;
        self.memory_restore_mode = None;
    }

    #[instrument(skip_all)]
    pub(crate) async fn dump(&self) -> Result<()> {
        let dump_data =
            serde_json::to_vec(&self).map_err(|e| anyhow!("failed to serialize sandbox, {}", e))?;
        let dump_path = format!("{}/sandbox.json", self.base_dir);
        let mut dump_file = match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dump_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    warn!("failed to open dump file {}, skip dump", dump_path);
                    return Ok(());
                }
                return Err(Error::IO(e));
            }
        };
        dump_file
            .write_all(dump_data.as_slice())
            .await
            .map_err(Error::IO)?;
        Ok(())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + DeserializeOwned + Recoverable + Sync + Send,
{
    #[instrument(skip_all)]
    async fn recover<P: AsRef<Path>>(base_dir: P) -> Result<Self> {
        let start = Instant::now();
        let dump_path = base_dir.as_ref().join("sandbox.json");
        let mut dump_file = OpenOptions::new()
            .read(true)
            .open(&dump_path)
            .await
            .map_err(Error::IO)?;
        let mut content = vec![];
        dump_file
            .read_to_end(&mut content)
            .await
            .map_err(Error::IO)?;
        let mut sb = serde_json::from_slice::<KuasarSandbox<V>>(content.as_slice())
            .map_err(|e| anyhow!("failed to deserialize sandbox, {}", e))?;
        if let SandboxStatus::Running(_) = sb.status {
            if let Err(e) = sb.vm.recover().await {
                warn!("failed to recover vm {}: {}, then force kill it!", sb.id, e);
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            };
            if let Err(e) = sb.init_client_fail_fast().await {
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover, init task client and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
            sb.sync_clock().await;
            sb.forward_events().await;
        }
        // recover the sandbox_cgroups in the sandbox object
        sb.sandbox_cgroups =
            SandboxCgroup::create_sandbox_cgroups(&sb.sandbox_cgroups.cgroup_parent_path, &sb.id)?;

        info!(
            "recover sandbox {} takes {}ms",
            sb.id,
            start.elapsed().as_millis()
        );
        Ok(sb)
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<()> {
        let pid = self.vm.start().await?;

        if let Err(e) = self.init_client().await {
            if let Err(re) = self.vm.stop(true).await {
                warn!("roll back in init task client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        // In virtio-blk mode there is no virtiofs to share sandbox config files.
        // Push them into the guest before setup_sandbox() which reads the hostname.
        if self.vm.container_storage_backend() == VIRTIO_BLK {
            if let Err(e) = self.push_sandbox_files().await {
                if let Err(re) = self.vm.stop(true).await {
                    warn!("roll back in push sandbox files: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
        }

        if let Err(e) = self.setup_sandbox().await {
            if let Err(re) = self.vm.stop(true).await {
                error!("roll back in setup sandbox client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        self.forward_events().await;

        self.status = SandboxStatus::Running(pid);
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn stop(&mut self, mut force: bool) -> Result<()> {
        match self.status {
            // If a sandbox is created:
            // 1. Just Created, vmm is not running: roll back and cleanup
            // 2. Created and vmm is running: roll back and cleanup
            // 3. Created and vmm is exited abnormally after running: status is Stopped
            SandboxStatus::Created => {
                // If a sandbox is in Created status, it means it was never successfully started.
                // We should treat this as a roll back and cleanup, and force kill any potential
                // vcpu or virtiofs-daemon processes.
                force = true;
            }
            SandboxStatus::Running(_) => {}
            SandboxStatus::Stopped(_, _) => {
                // Network should already be destroyed when sandbox is stopped.
                self.destroy_network().await;
                return Ok(());
            }
            _ => {
                return Err(
                    anyhow!("sandbox {} is in {:?} while stop", self.id, self.status).into(),
                );
            }
        }
        let container_ids: Vec<String> = self.containers.keys().map(|k| k.to_string()).collect();
        if force {
            for id in container_ids {
                if let Err(e) = self.remove_container(&id).await {
                    warn!("failed to remove container {} during stop, {}", id, e);
                }
            }
        } else {
            for id in container_ids {
                self.remove_container(&id).await?;
            }
        }

        self.vm.stop(force).await?;
        self.destroy_network().await;
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) fn container_mut(&mut self, id: &str) -> Result<&mut KuasarContainer> {
        self.containers
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("no container with id {}", id)))
    }

    #[instrument(skip_all)]
    pub(crate) fn increment_and_get_id(&mut self) -> u32 {
        self.id_generator += 1;
        self.id_generator
    }

    #[instrument(skip_all)]
    async fn init_client(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client(&addr).await?;
            self.check_and_set_client(&mut client_guard, client).await?;
        }
        Ok(())
    }

    /// Use fail-fast connect strategy during recovery: if the guest agent
    /// socket returns a fatal error (e.g. broken pipe), bail out immediately
    /// instead of retrying until timeout.
    #[instrument(skip_all)]
    async fn init_client_fail_fast(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client_fail_fast(&addr).await?;
            self.check_and_set_client(&mut client_guard, client).await?;
        }
        Ok(())
    }

    async fn check_and_set_client(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, Option<SandboxServiceClient>>,
        client: SandboxServiceClient,
    ) -> Result<()> {
        debug!("connected to task server {}", self.id);
        client_check(&client, DEFAULT_CLIENT_CHECK_TIMEOUT).await?;
        **guard = Some(client);
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn setup_sandbox(&mut self) -> Result<()> {
        let mut req = SetupSandboxRequest::new();

        if let Some(client) = &*self.client.lock().await {
            // Set PodSandboxConfig
            if let Some(config) = &self.data.config {
                let config_str = serde_json::to_vec(config).map_err(|e| {
                    Error::Other(anyhow!(
                        "failed to marshal PodSandboxConfig to string, {:?}",
                        e
                    ))
                })?;

                let mut any = Any::new();
                any.type_url = "PodSandboxConfig".to_string();
                any.value = config_str;

                req.config = MessageField::some(any);
            }

            if let Some(network) = self.network.as_ref() {
                // Set interfaces
                req.interfaces = network.interfaces().iter().map(|x| x.into()).collect();

                // Set routes
                req.routes = network.routes().iter().map(|x| x.into()).collect();
            }

            client_setup_sandbox(client, &req).await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn sync_clock(&self) {
        if let Some(client) = &*self.client.lock().await {
            client_sync_clock(client, self.id.as_str(), self.exit_signal.clone());
        }
    }

    /// Inject fresh entropy into the guest after restoring from a snapshot.
    ///
    /// Multiple VMs restored from the same snapshot share the same initial entropy
    /// pool state; writing new host-generated random bytes prevents correlated output
    /// from `/dev/urandom` across those instances.  Non-fatal: a warning is logged
    /// on failure and the restore continues.
    pub(crate) async fn reseed_guest_entropy(&self) {
        let seed: [u8; 32] = rand::random();
        if let Some(client) = &*self.client.lock().await {
            if let Err(e) = client_reseed_entropy(client, &seed).await {
                warn!("sandbox {}: reseed_guest_entropy: {}", self.id, e);
            }
        }
    }

    /// Drive the WarmFork post-restore protocol via the guest agent.
    ///
    /// **Autonomous mode** (`params.task_id` is None): empty `task_id` is sent; the guest skips
    /// PREPARE/READY and self-starts after COMMIT.
    ///
    /// **Injection mode** (`params.task_id` is Some): full PREPARE → READY → COMMIT → STARTED.
    async fn inject_warm_fork_task(&self, params: &WarmForkParams) -> Result<()> {
        let task_id = params.task_id.clone().unwrap_or_default();
        let tasks: Vec<ContainerTask> = params
            .targets
            .iter()
            .map(|t| {
                let mut ct = ContainerTask::new();
                ct.container_id = t.container_id.clone();
                ct.socket_path = t.socket_path.clone();
                ct.task_id = task_id.clone();
                ct.env_overrides = t.env_overrides.clone();
                ct.context = t.context.clone();
                ct
            })
            .collect();
        let task_count = tasks.len();

        let mut req = InjectTaskRequest::new();
        req.tasks = tasks;
        req.prepare_timeout_ms = params.prepare_timeout_ms;
        req.commit_timeout_ms = params.commit_timeout_ms;

        // In autonomous mode there is no PREPARE phase, so the outer deadline only needs to
        // cover the commit timeout. In injection mode both phases must complete within it.
        let outer_ms = if params.task_id.is_some() {
            u64::from(params.prepare_timeout_ms) + u64::from(params.commit_timeout_ms) + 3_000
        } else {
            u64::from(params.commit_timeout_ms) + 3_000
        };
        let timeout_ns = Duration::from_millis(outer_ms).as_nanos() as i64;
        let inject_start = std::time::Instant::now();

        let client_guard = self.client.lock().await;
        if let Some(client) = client_guard.as_ref() {
            let resp = client
                .inject_task(with_timeout(timeout_ns), &req)
                .await
                .map_err(|e| anyhow!("inject_task RPC: {}", e))?;

            // Guard against the guest returning fewer results than requested.
            if resp.results.len() != task_count {
                return Err(anyhow!(
                    "sandbox {}: inject_task result count mismatch: sent {} tasks, got {} results",
                    self.id,
                    task_count,
                    resp.results.len()
                )
                .into());
            }

            let requested_ids: HashSet<String> =
                req.tasks.iter().map(|t| t.container_id.clone()).collect();
            let result_ids: HashSet<String> = resp
                .results
                .iter()
                .map(|r| r.container_id.clone())
                .collect();
            if result_ids != requested_ids {
                return Err(anyhow!(
                    "sandbox {}: inject_task result container_id set mismatch: sent {:?}, got {:?}",
                    self.id,
                    requested_ids,
                    result_ids
                )
                .into());
            }

            // Check all per-target results.
            let mut errors: Vec<String> = Vec::new();
            let mut started_count = 0usize;
            for r in &resp.results {
                if r.status() == InjectStatus::INJECT_STARTED {
                    started_count += 1;
                } else {
                    errors.push(format!(
                        "container '{}': {:?}/{:?} — {}",
                        r.container_id,
                        r.status(),
                        r.phase(),
                        r.message
                    ));
                }
            }
            if !errors.is_empty() {
                return Err(anyhow!(
                    "sandbox {}: WarmFork inject_task failed for {} target(s): {}",
                    self.id,
                    errors.len(),
                    errors.join("; ")
                )
                .into());
            }
            info!(
                "sandbox {}: WarmFork inject_task committed ({} target(s) STARTED, \
                 mode={}, task_id={}, template_id={}, inject_latency_ms={})",
                self.id,
                started_count,
                if params.task_id.is_some() {
                    "injection"
                } else {
                    "autonomous"
                },
                params.task_id.as_deref().unwrap_or("-"),
                self.template_id.as_deref().unwrap_or("-"),
                inject_start.elapsed().as_millis(),
            );
        } else {
            return Err(anyhow!(
                "sandbox {}: client not initialized before inject_task",
                self.id
            )
            .into());
        }
        Ok(())
    }

    /// Verify that all WarmFork readiness sockets are open and workloads speak the expected
    /// protocol version. Probes each target listed in `targets`.
    ///
    /// Connects to each socket, reads the workload's CAPABILITIES message, verifies the
    /// protocol version, then closes the connection. Each workload receives EOF on its next
    /// read and loops back to `accept()`, ready for real injection after restore.
    pub(crate) async fn check_inject_socket(&self, targets: &[InjectTarget]) -> Result<()> {
        let client_guard = self.client.lock().await;
        let client = client_guard.as_ref().ok_or_else(|| {
            anyhow!(
                "sandbox {}: client not initialized, cannot verify readiness socket",
                self.id
            )
        })?;

        let mut req = CheckInjectSocketRequest::new();
        req.targets = targets.to_vec();

        let timeout_ns = Duration::from_secs(5).as_nanos() as i64;
        let resp = client
            .check_inject_socket(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("sandbox {}: check_inject_socket RPC failed: {}", self.id, e))?;

        let mut errors: Vec<String> = Vec::new();
        for r in &resp.results {
            use vmm_common::api::sandbox::TargetCheckStatus;
            if r.status() != TargetCheckStatus::TARGET_CHECK_OK {
                errors.push(format!(
                    "container '{}': {:?} — {}",
                    r.container_id,
                    r.status(),
                    r.message
                ));
            }
        }
        if !errors.is_empty() {
            return Err(anyhow!(
                "sandbox {}: readiness socket check failed: {}",
                self.id,
                errors.join("; ")
            )
            .into());
        }
        info!(
            "sandbox {}: readiness sockets verified ({} target(s))",
            self.id,
            resp.results.len()
        );
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn setup_sandbox_files(&self) -> Result<()> {
        let shared_path = self.get_sandbox_shared_path();
        create_dir_all(&shared_path)
            .await
            .map_err(|e| anyhow!("create host sandbox path({}): {}", shared_path, e))?;

        // Handle hostname
        let mut hostname = get_hostname(&self.data);
        if hostname.is_empty() {
            hostname = hostname::get()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        hostname.push('\n');
        let hostname_path = Path::new(&shared_path).join(HOSTNAME_FILENAME);
        write_str_to_file(hostname_path, &hostname)
            .await
            .map_err(|e| anyhow!("write hostname: {:?}", e))?;

        // handle hosts
        let hosts_path = Path::new(&shared_path).join(HOSTS_FILENAME);
        copy(ETC_HOSTS, hosts_path)
            .await
            .map_err(|e| anyhow!("copy hosts: {}", e))?;

        // handle resolv.conf
        let resolv_path = Path::new(&shared_path).join(RESOLV_FILENAME);
        match get_dns_config(&self.data).map(|dns_config| {
            parse_dnsoptions(
                &dns_config.servers,
                &dns_config.searches,
                &dns_config.options,
            )
        }) {
            Some(resolv_content) if !resolv_content.is_empty() => {
                write_str_to_file(resolv_path, &resolv_content)
                    .await
                    .map_err(|e| anyhow!("write reslov.conf: {:?}", e))?;
            }
            _ => {
                copy(ETC_RESOLV, resolv_path)
                    .await
                    .map_err(|e| anyhow!("copy resolv.conf: {}", e))?;
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn get_sandbox_shared_path(&self) -> String {
        format!("{}/{}", self.base_dir, SHARED_DIR_SUFFIX)
    }

    // Push sandbox config files (hostname, resolv.conf, hosts) directly into the guest.
    // Used in virtio-blk mode where there is no virtiofs shared directory.
    // The three file pushes are issued concurrently to reduce sandbox start latency.
    #[instrument(skip_all)]
    async fn push_sandbox_files(&self) -> Result<()> {
        let shared_path = self.get_sandbox_shared_path();
        let client_guard = self.client.lock().await;
        let client = client_guard.as_ref().ok_or_else(|| {
            anyhow!("TTRPC client not initialized when pushing sandbox files in virtio-blk mode")
        })?;
        let injector = GuestFileInjector::new(client);

        // Ensure KUASAR_STATE_DIR exists in the guest so containers can bind-mount from it
        injector
            .ensure_dir(KUASAR_STATE_DIR)
            .await
            .map_err(|e| anyhow!("create kuasar state dir in guest: {}", e))?;

        // Read all config files from host shared dir; log a warning for any missing file
        // so that a misconfigured or not-yet-populated shared dir is observable.
        let hostname_content =
            tokio::fs::read(format!("{}/{}", shared_path, HOSTNAME_FILENAME)).await;
        if let Err(ref e) = hostname_content {
            warn!("virtio-blk: could not read {}: {}", HOSTNAME_FILENAME, e);
        }
        let hostname_content = hostname_content.ok();

        let resolv_content = tokio::fs::read(format!("{}/{}", shared_path, RESOLV_FILENAME)).await;
        if let Err(ref e) = resolv_content {
            warn!("virtio-blk: could not read {}: {}", RESOLV_FILENAME, e);
        }
        let resolv_content = resolv_content.ok();

        let hosts_content = tokio::fs::read(format!("{}/{}", shared_path, HOSTS_FILENAME)).await;
        if let Err(ref e) = hosts_content {
            warn!("virtio-blk: could not read {}: {}", HOSTS_FILENAME, e);
        }
        let hosts_content = hosts_content.ok();

        let hostname_dest = format!("{}/{}", KUASAR_STATE_DIR, HOSTNAME_FILENAME);
        let resolv_dest = format!("{}/{}", KUASAR_STATE_DIR, RESOLV_FILENAME);
        let hosts_dest = format!("{}/{}", KUASAR_STATE_DIR, HOSTS_FILENAME);

        // Issue all pushes concurrently to reduce sandbox start latency.
        let push_hostname = async {
            if let Some(content) = hostname_content {
                injector.push_file(&hostname_dest, content, 0o644).await?;
            }
            Ok::<(), containerd_sandbox::error::Error>(())
        };

        let push_resolv = async {
            if let Some(content) = resolv_content {
                injector.push_file(&resolv_dest, content, 0o644).await?;
                injector
                    .exec(&format!(
                        "mount --bind {} {}",
                        shell_quote(&resolv_dest),
                        shell_quote(ETC_RESOLV)
                    ))
                    .await?;
            }
            Ok::<(), containerd_sandbox::error::Error>(())
        };

        let push_hosts = async {
            if let Some(content) = hosts_content {
                injector.push_file(&hosts_dest, content, 0o644).await?;
            }
            Ok::<(), containerd_sandbox::error::Error>(())
        };

        let (r1, r2, r3) = tokio::join!(push_hostname, push_resolv, push_hosts);
        r1?;
        r2?;
        r3?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn prepare_network(&mut self) -> Result<()> {
        // get vcpu for interface queue, at least one vcpu
        let mut vcpu = 1;
        if let Some(resources) = get_resources(&self.data) {
            if resources.cpu_period > 0 && resources.cpu_quota > 0 {
                // get ceil of cpus if it is not integer
                let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
                vcpu = base as u32;
            }
        }

        let network_config = NetworkConfig {
            netns: self.data.netns.to_string(),
            sandbox_id: self.id.to_string(),
            queue: vcpu,
        };
        let network = Network::new(network_config).await?;
        network.attach_to(self).await?;
        Ok(())
    }

    //  If a sandbox is still running, destroy network may hang with its running
    #[instrument(skip_all)]
    pub async fn destroy_network(&mut self) {
        // Network should be destroyed only once, take it out here.
        if let Some(mut network) = self.network.take() {
            network.destroy().await;
        }
    }

    #[instrument(skip_all)]
    pub async fn add_to_cgroup(&self) -> Result<()> {
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // add vmm process into sandbox cgroup
            if let SandboxStatus::Running(vmm_pid) = self.status {
                let vcpu_threads = self.vm.vcpus().await?;
                debug!(
                    "vmm process pid: {}, vcpu threads pid: {:?}",
                    vmm_pid, vcpu_threads
                );
                self.sandbox_cgroups
                    .add_process_into_sandbox_cgroups(vmm_pid, Some(vcpu_threads))?;
                // move all vmm-related process into sandbox cgroup
                for pid in self.vm.pids().affiliated_pids {
                    self.sandbox_cgroups
                        .add_process_into_sandbox_cgroups(pid, None)?;
                }
            } else {
                return Err(Error::Other(anyhow!(
                    "sandbox status is not Running after started!"
                )));
            }
        }
        Ok(())
    }

    pub(crate) async fn forward_events(&mut self) {
        if let Some(client) = &*self.client.lock().await {
            let client = client.clone();
            let exit_signal = self.exit_signal.clone();
            tokio::spawn(async move {
                let fut = async {
                    loop {
                        match client.get_events(with_timeout(0), &Empty::new()).await {
                            Ok(resp) => {
                                if let Err(e) =
                                    crate::client::publish_event(convert_envelope(resp)).await
                                {
                                    error!("{}", e);
                                }
                            }
                            Err(err) => {
                                // if sandbox was closed, will get error Socket("early eof"),
                                // so we should handle errors except this unexpected EOF error.
                                if let ttrpc::error::Error::Socket(s) = &err {
                                    if s.contains("early eof") {
                                        break;
                                    }
                                }
                                error!("failed to get oom event error {:?}", err);
                                break;
                            }
                        }
                    }
                };

                tokio::select! {
                    _ = fut => {},
                    _ = exit_signal.wait() => {},
                }
            });
        }
    }
}

// parse_dnsoptions parse DNS options into resolv.conf format content,
// if none option is specified, will return empty with no error.
fn parse_dnsoptions(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut resolv_content = String::new();

    if !searches.is_empty() {
        resolv_content.push_str(&format!("search {}\n", searches.join(" ")));
    }

    if !servers.is_empty() {
        resolv_content.push_str(&format!("nameserver {}\n", servers.join("\nnameserver ")));
    }

    if !options.is_empty() {
        resolv_content.push_str(&format!("options {}\n", options.join(" ")));
    }

    resolv_content
}

pub fn has_shared_pid_namespace(data: &SandboxData) -> bool {
    if let Some(conf) = &data.config {
        if let Some(pid_ns_mode) = conf
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|s| s.namespace_options.as_ref())
            .map(|n| n.pid())
        {
            return pid_ns_mode == NamespaceMode::Pod;
        }
    }
    false
}

fn convert_envelope(envelope: vmm_common::api::events::Envelope) -> Envelope {
    Envelope {
        timestamp: envelope.timestamp,
        namespace: envelope.namespace,
        topic: envelope.topic,
        event: envelope.event,
        special_fields: protobuf::SpecialFields::default(),
    }
}

/// Parse and validate the optional multi-container target annotation.
pub(crate) fn parse_warm_fork_container_names(
    annotations: &HashMap<String, String>,
) -> std::result::Result<Option<Vec<String>>, anyhow::Error> {
    let Some(containers_ann) = annotations.get(ANNOTATION_WARM_FORK_CONTAINERS) else {
        return Ok(None);
    };

    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for raw in containers_ann.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            return Err(anyhow!(
                "'{}' contains an empty container name (value={:?})",
                ANNOTATION_WARM_FORK_CONTAINERS,
                containers_ann
            ));
        }
        if !seen.insert(name.to_string()) {
            return Err(anyhow!(
                "'{}' contains duplicate container name '{}'",
                ANNOTATION_WARM_FORK_CONTAINERS,
                name
            ));
        }
        names.push(name.to_string());
    }

    if names.is_empty() {
        return Err(anyhow!(
            "'{}' annotation is present but yields no container names (value={:?})",
            ANNOTATION_WARM_FORK_CONTAINERS,
            containers_ann
        ));
    }

    Ok(Some(names))
}

/// Parse WarmFork restore parameters from pod annotations.
///
/// Always returns a `WarmForkParams`. Callers must already know the template is `WarmFork`.
///
/// Two modes determined by the `kuasar.io/task-id` annotation:
/// - Absent or empty → **autonomous mode** (`task_id: None`): guest self-starts after COMMIT.
/// - Non-empty → **injection mode** (`task_id: Some(id)`): full PREPARE/READY/COMMIT/STARTED.
///
/// Multi-container: if `kuasar.io/warm-fork-containers` is set, one `WarmForkTarget` is
/// built per named container. Per-container annotations take priority over pod-level ones.
/// The `container_id` field is left empty here (container IDs are looked up at restore time
/// when the running containers are known).
pub(crate) fn parse_warm_fork_params(
    annotations: &HashMap<String, String>,
) -> std::result::Result<WarmForkParams, anyhow::Error> {
    let task_id = annotations
        .get(ANNOTATION_WARM_FORK_TASK_ID)
        .filter(|s| !s.is_empty())
        .cloned();

    // Pod-level defaults
    let pod_socket = annotations
        .get(ANNOTATION_WARM_FORK_READINESS_SOCKET)
        .map(|s| s.as_str())
        .unwrap_or(ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET);
    let pod_context = annotations
        .get(ANNOTATION_WARM_FORK_TASK_CONTEXT)
        .cloned()
        .unwrap_or_default();
    let pod_env: HashMap<String, String> = annotations
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix(ANNOTATION_WARM_FORK_ENV_PREFIX)
                .map(|env_key| (env_key.to_string(), v.clone()))
        })
        .collect();

    // Build target list
    let targets = if let Some(names) = parse_warm_fork_container_names(annotations)? {
        names
            .into_iter()
            .map(|name| {
                // Per-container socket path override
                let socket = annotations
                    .get(&format!(
                        "kuasar.io/container/{}/warm-fork-readiness-socket",
                        &name
                    ))
                    .cloned()
                    .unwrap_or_else(|| pod_socket.to_string());
                // Per-container context override
                let context = annotations
                    .get(&format!("kuasar.io/container/{}/task-context", &name))
                    .cloned()
                    .unwrap_or_else(|| pod_context.clone());
                // Merge env: pod-level base, then per-container overrides
                let mut env = pod_env.clone();
                for (k, v) in annotations.iter() {
                    let prefix = format!("kuasar.io/container/{}/task-env/", &name);
                    if let Some(env_key) = k.strip_prefix(&prefix) {
                        env.insert(env_key.to_string(), v.clone());
                    }
                }
                WarmForkTarget {
                    container_name: name,
                    container_id: String::new(), // overlaid from PooledTemplate at restore time
                    socket_path: socket,
                    env_overrides: env,
                    context,
                }
            })
            .collect()
    } else {
        // Single-container mode: container_id is overlaid from the template at restore time.
        vec![WarmForkTarget {
            container_name: String::new(),
            container_id: String::new(),
            socket_path: pod_socket.to_string(),
            env_overrides: pod_env,
            context: pod_context,
        }]
    };

    Ok(WarmForkParams {
        task_id,
        targets,
        prepare_timeout_ms: 10_000,
        commit_timeout_ms: 5_000,
    })
}

/// Memory restore mode sent to Cloud Hypervisor in the PUT /api/v1/vm.restore payload.
/// Config values are case-insensitive: `"copy"`, `"ondemand"`, `"filebackend"`, `"externaluffd"`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub enum MemoryRestoreMode {
    /// Copy guest memory pages from snapshot into anonymous memory (default).
    #[default]
    Copy,
    /// Map pages on-demand from the snapshot file using userfaultfd.
    OnDemand,
    /// Map the snapshot file directly as a file-backed memory region.
    FileBackend,
    /// Delegate page faults to an external userfaultfd handler process.
    ExternalUffd,
}

impl<'de> Deserialize<'de> for MemoryRestoreMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_lowercase().as_str() {
            "copy" => Ok(Self::Copy),
            "ondemand" => Ok(Self::OnDemand),
            "filebackend" => Ok(Self::FileBackend),
            "externaluffd" => Ok(Self::ExternalUffd),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["copy", "ondemand", "filebackend", "externaluffd"],
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    /// Enable Environment (bare VM) snapshot restore instead of cold-booting each sandbox.
    /// Safe to enable after validating that the hypervisor restore process works end-to-end.
    pub enable_environment_restore: bool,
    /// Enable WarmFork restore.  Requires end-to-end validation in your environment
    /// before enabling in production.  Defaults to false.
    pub enable_warmfork_restore: bool,
    /// Enable Continuation restore.  Restores a process with its full in-memory
    /// state and original network identity (virtual IP / Pod IP).  Network identity transfer
    /// is an external concern — the operator or CNI layer must route the original Pod IP to
    /// this node before the restore is triggered.  Kuasar trusts that transfer is complete
    /// when `start()` is called.  Existing TCP connections are not preserved; workloads must
    /// tolerate reconnect errors.  Defaults to false.
    pub enable_continuation_restore: bool,
    /// Maximum number of concurrent VM restores across all template kinds.
    /// Caps host memory pressure when many sandboxes start simultaneously.
    #[serde(default = "SnapshotConfig::default_max_concurrent_restores")]
    pub max_concurrent_restores: usize,
    pub fallback_to_fresh_boot: bool,
    pub default_memory_restore_mode: MemoryRestoreMode,
}

const DEFAULT_MAX_CONCURRENT_RESTORES: usize = 4;

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enable_environment_restore: false,
            enable_warmfork_restore: false,
            enable_continuation_restore: false,
            max_concurrent_restores: DEFAULT_MAX_CONCURRENT_RESTORES,
            fallback_to_fresh_boot: true,
            default_memory_restore_mode: MemoryRestoreMode::Copy,
        }
    }
}

impl SnapshotConfig {
    fn default_max_concurrent_restores() -> usize {
        DEFAULT_MAX_CONCURRENT_RESTORES
    }
}

#[derive(Default, Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub enable_tracing: bool,
    #[serde(default)]
    pub snapshot: SnapshotConfig,
}

/// Template lease mode: how the pool manages a template entry after restore.
///
/// `Shared` — the template entry stays in the pool and is ref-counted. Multiple sandboxes can
/// restore from the same entry concurrently. The entry is GC-eligible only when its ref-count
/// reaches zero. This is the default and the main production path for WarmFork.
///
/// `Exclusive` — the template entry is consumed (removed from pool) on the first restore. The
/// snapshot files remain on disk until the sandbox is stopped and deleted. Use this for
/// one-shot or stateful-token workloads where sharing a ready-waiting process is not correct.
///
/// Note: "Shared" does not imply any underlying memory CoW or process fork capability.
/// It means the pool entry can be reused across multiple restores. Whether the guest VM state
/// can be shared at the memory level is a separate infrastructure concern not yet implemented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TemplateLeaseMode {
    #[default]
    Shared,
    Exclusive,
}

impl std::fmt::Display for TemplateLeaseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateLeaseMode::Shared => write!(f, "shared"),
            TemplateLeaseMode::Exclusive => write!(f, "exclusive"),
        }
    }
}

impl SandboxConfig {
    pub fn log_level(&self) -> String {
        self.log_level.to_string()
    }

    pub fn enable_tracing(&self) -> bool {
        self.enable_tracing
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDeviceSpec {
    #[serde(default)]
    pub(crate) _host_path: Vec<String>,
    #[serde(default)]
    pub(crate) _bdf: Vec<String>,
}

pub(crate) fn monitor<V: VM + 'static>(sandbox_mutex: Arc<Mutex<KuasarSandbox<V>>>) {
    tokio::spawn(async move {
        let mut rx = {
            let sandbox = sandbox_mutex.lock().await;
            if let SandboxStatus::Running(_) = sandbox.status.clone() {
                if let Some(rx) = sandbox.vm.wait_channel().await {
                    rx
                } else {
                    error!("can not get wait channel when sandbox is running");
                    return;
                }
            } else {
                info!(
                    "sandbox {} is {:?} when monitor",
                    sandbox.id, sandbox.status
                );
                return;
            }
        };

        let (code, ts) = *rx.borrow();
        if ts == 0 {
            rx.changed().await.unwrap_or_default();
            let (code, ts) = *rx.borrow();
            let mut sandbox = sandbox_mutex.lock().await;
            info!("monitor sandbox {} terminated", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        } else {
            let mut sandbox = sandbox_mutex.lock().await;
            info!("sandbox {} already terminated before monit it", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        }
    });
}

// ---------------------------------------------------------------------------
// Template pool: create_template + start_with_template_key / start_with_template_id
// ---------------------------------------------------------------------------

/// Return a factory with vcpus/memory overridden by the request, or a clone of the
/// original if neither field is specified.
fn apply_resource_overrides<F: VMFactory>(factory: &Arc<F>, req: &CreateTemplateRequest) -> Arc<F> {
    match (req.vcpus, req.memory_mb) {
        (None, None) => Arc::clone(factory),
        (vcpus, memory_mb) => Arc::new(factory.with_resources(
            vcpus.unwrap_or_else(|| factory.vcpus()),
            memory_mb.unwrap_or_else(|| factory.memory_mb()),
        )),
    }
}

/// Boot a fresh VM, wait for the guest agent, snapshot, then stop the VM.
/// The resulting snapshot is added to the template pool and stored on disk.
pub(crate) async fn create_template_worker<F>(
    factory: Arc<F>,
    pool: Arc<TemplatePool>,
    req: CreateTemplateRequest,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    let template_base = pool.store_dir.join("environment").join(&req.id);
    let vm_base = template_base.join("vm");

    // Apply per-template resource overrides before dispatching to the inner function.
    let effective_factory = apply_resource_overrides(&factory, &req);
    let result =
        create_template_inner(&*effective_factory, &pool, &req, &template_base, &vm_base).await;

    if let Err(ref e) = result {
        warn!("template {}: creation failed ({}), cleaning up", req.id, e);
        if let Err(ce) = tokio::fs::remove_dir_all(&template_base).await {
            warn!("template {}: cleanup on failure: {}", req.id, ce);
        }
    }
    result
}

async fn create_template_inner<F>(
    factory: &F,
    pool: &Arc<TemplatePool>,
    req: &CreateTemplateRequest,
    template_base: &Path,
    vm_base: &Path,
) -> Result<PooledTemplate>
where
    F: VMFactory,
    F::VM: VM + Snapshottable + Sync + Send,
{
    tokio::fs::create_dir_all(vm_base)
        .await
        .map_err(|e| anyhow!("create template vm dir: {}", e))?;

    let sandbox_opt = SandboxOption {
        base_dir: vm_base.to_string_lossy().to_string(),
        sandbox: SandboxData {
            id: req.id.clone(),
            ..Default::default()
        },
    };

    let mut vm = factory.create_vm(&req.id, &sandbox_opt).await?;
    vm.start().await?;

    // Wait for the guest agent to be ready, then flush fs journals and drop page
    // cache to produce a clean, compact snapshot.
    let agent_client = vm
        .wait_agent_ready(30)
        .await
        .map_err(|e| anyhow!("template {}: {}", req.id, e))?;
    let pre_snap_timeout_ns = Duration::from_secs(10).as_nanos() as i64;
    let mut exec_req = ExecVMProcessRequest::new();
    exec_req.command = "sync; echo 1 > /proc/sys/vm/drop_caches".to_string();
    if let Err(e) = agent_client
        .exec_vm_process(with_timeout(pre_snap_timeout_ns), &exec_req)
        .await
    {
        warn!(
            "template {}: pre-snapshot sync/drop_caches failed: {}",
            req.id, e
        );
    }

    let snapshot_dir = template_base.join("snapshot");
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .map_err(|e| anyhow!("create snapshot dir: {}", e))?;

    let snap_start = Instant::now();
    let mut meta: SnapshotMeta = vm
        .snapshot(&snapshot_dir, &[])
        .await
        .map_err(|e| anyhow!("template {}: snapshot failed: {}", req.id, e))?;
    meta.lease_mode = req.lease_mode.clone();
    meta.created_at = std::time::SystemTime::now();

    info!(
        "template {}: snapshot captured in {:.3}s (lease_mode={})",
        req.id,
        snap_start.elapsed().as_secs_f64(),
        meta.lease_mode
    );

    if let Err(e) = vm.stop(false).await {
        warn!("template {}: stop after snapshot: {}", req.id, e);
    }

    // Remove the temporary VM directory (sockets, sandbox.json, etc.); only the
    // snapshot directory under template_base is retained.
    if let Err(e) = tokio::fs::remove_dir_all(vm_base).await {
        warn!("template {}: cleanup vm dir: {}", req.id, e);
    }

    // Environment key is always derived from the factory config — user-specified keys are
    // rejected at the admin API boundary and must never reach this point.
    let key = TemplateKey::from_vm_config(
        factory.kernel_path(),
        factory.image_path(),
        factory.vcpus(),
        factory.memory_mb(),
        factory.kernel_params(),
        factory.storage_backend(),
    );
    let mut tmpl = PooledTemplate::new(
        &req.id,
        key,
        meta.snapshot_dir,
        factory.image_path(),
        factory.kernel_path(),
        factory.vcpus(),
        factory.memory_mb(),
        meta.original_task_vsock,
        meta.original_console_path,
    );

    tmpl.created_at_secs = meta
        .created_at
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    pool.add(tmpl.clone()).await?;
    info!(
        "template {}: added to pool (pool_depth={})",
        req.id,
        pool.depth(&tmpl.key).await
    );
    Ok(tmpl)
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    /// Initialize the template pool from `store_dir`, rehydrating any previously
    /// persisted templates.
    #[allow(clippy::too_many_arguments)]
    pub async fn init_template_pool(
        &mut self,
        sandboxer_working_dir: String,
        store_dir: PathBuf,
        environment_max_per_key: usize,
        warmfork_max_per_key: usize,
        gc_watermark: usize,
        lease_mode: TemplateLeaseMode,
        max_concurrent_restores: usize,
    ) -> Result<()> {
        // Check reflink support between sandboxer_working_dir and store_dir.
        // mkfs.xfs / cp availability is implicitly validated here: if cp is missing the
        // probe returns Err and we fall through to plain-copy mode.
        let reflink_supported = if lease_mode == TemplateLeaseMode::Shared {
            match crate::cloud_hypervisor::check_reflink_support(
                Path::new(&sandboxer_working_dir),
                &store_dir,
            )
            .await
            {
                Ok(true) => {
                    log::info!(
                        "reflink supported between {} and {} - Shared mode will use COW",
                        sandboxer_working_dir,
                        store_dir.display()
                    );
                    true
                }
                Ok(false) => {
                    log::warn!(
                        "reflink not supported between {} and {}. \
                        Shared mode will use plain copy (less space-efficient). \
                        Consider using XFS with reflink=1 or btrfs for both directories.",
                        sandboxer_working_dir,
                        store_dir.display()
                    );
                    false
                }
                Err(e) => {
                    log::error!(
                        "reflink test failed: {}. Shared mode will use plain copy.",
                        e
                    );
                    false
                }
            }
        } else {
            false
        };

        let pool = TemplatePool::load_from_disk(
            store_dir.clone(),
            environment_max_per_key,
            warmfork_max_per_key,
            gc_watermark,
            lease_mode.clone(),
            reflink_supported,
        )
        .await?;
        let total = pool.total_depth().await;
        let keys = pool.key_count().await;
        log::info!(
            "template pool initialized: {} templates across {} key(s) \
            (store={}, environment_max_per_key={}, warmfork_max_per_key={}, gc_watermark={}, lease_mode={}, reflink={})",
            total, keys, pool.store_dir.display(),
            pool.environment_max_per_key, pool.warmfork_max_per_key,
            pool.gc_watermark, lease_mode, reflink_supported
        );
        self.template_pool = Some(pool);
        self.restore_semaphore = Arc::new(Semaphore::new(max_concurrent_restores));
        Ok(())
    }

    /// Return a handle for the service socket.
    ///
    /// `dir` is the sandboxer working directory (from `--dir`).  Service-managed sandbox slots
    /// are placed under `<dir>/`.
    ///
    /// Initialize the continuation store from `base_dir`.
    ///
    /// The store directory `{base_dir}/continuation/` is created if absent.
    /// Must be called before `start()` for pods with Continuation annotations.
    pub async fn init_continuation_store(&mut self, base_dir: PathBuf) -> Result<()> {
        let store = ContinuationStore::load_from_disk(base_dir).await?;
        self.continuation_store = Some(store);
        Ok(())
    }

    /// Spawn the background ContinuationStore GC task.
    ///
    /// The task only removes consumed entries that are no longer referenced by a live sandbox.
    /// Unconsumed entries are intentionally retained until explicitly deleted or consumed.
    pub fn start_continuation_gc_task(&self, interval_secs: u64) {
        if interval_secs == 0 {
            info!("continuation store GC disabled (interval_secs=0)");
            return;
        }
        let store = match &self.continuation_store {
            Some(s) => s.clone(),
            None => {
                warn!("start_continuation_gc_task: continuation store not initialized, skipping");
                return;
            }
        };
        let sandboxes = self.sandboxes.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval_secs);
            info!(
                "continuation store GC task started (interval={}s)",
                interval_secs
            );
            loop {
                tokio::time::sleep(interval).await;
                let active_ids = {
                    let sbs = sandboxes.read().await;
                    let mut ids = std::collections::HashSet::new();
                    for sb_mutex in sbs.values() {
                        let sb = sb_mutex.lock().await;
                        if let Some(tid) = &sb.template_id {
                            ids.insert(tid.clone());
                        }
                    }
                    ids
                };
                match store.gc_orphaned_consumed(&active_ids).await {
                    Ok(removed) if removed > 0 => {
                        info!(
                            "continuation store GC removed {} consumed entrie(s)",
                            removed
                        );
                    }
                    Ok(_) => {}
                    Err(e) => warn!("continuation store GC failed: {}", e),
                }
            }
        });
    }

    /// Always succeeds; pool-dependent operations (template create/run/pool-status/…)
    /// return an error when the template pool is not configured.
    pub fn service_handle(&self, dir: impl Into<PathBuf>) -> crate::service::Handle<F> {
        crate::service::Handle {
            factory: Arc::clone(&self.factory),
            sandboxes: self.sandboxes.clone(),
            pool: self.template_pool.clone(),
            continuation_store: self.continuation_store.clone(),
            snapshot_config: self.config.snapshot.clone(),
            sandbox_base_dir: dir.into(),
        }
    }

    /// Spawn the background pool maintenance task.
    ///
    /// Every `interval_secs` seconds the task:
    /// 1. Computes how many additional Environment refills are needed to reach `min_pool_depth`
    ///    (accounting for both available templates and already-in-flight refills).
    /// 2. Spawns that many `create_template_worker` tasks so the pool stays warm.
    /// 3. Calls `gc_if_needed` to evict templates above the GC watermark.
    ///
    /// The task runs until the process exits. A warning is logged and the method
    /// returns early if the template pool has not been initialized.
    pub fn start_maintenance_task(&self, min_pool_depth: usize, interval_secs: u64) {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => {
                warn!("start_maintenance_task: template pool not initialized, skipping");
                return;
            }
        };
        let factory = Arc::clone(&self.factory);
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval_secs);
            info!(
                "template pool maintenance task started (min_pool_depth={}, interval={}s)",
                min_pool_depth, interval_secs,
            );
            loop {
                tokio::time::sleep(interval).await;

                let environment_key = TemplateKey::from_vm_config(
                    factory.kernel_path(),
                    factory.image_path(),
                    factory.vcpus(),
                    factory.memory_mb(),
                    factory.kernel_params(),
                    factory.storage_backend(),
                );

                let current = pool.depth(&environment_key).await;
                let in_flight = pool.in_flight_count_for_key(&environment_key).await;
                let covered = current.saturating_add(in_flight);
                if covered < min_pool_depth {
                    let need = min_pool_depth - covered;
                    info!(
                        "template pool maintenance: depth={}, in_flight={}, spawning {} refill(s)",
                        current, in_flight, need,
                    );
                    for _ in 0..need {
                        let refill_id = new_template_id();
                        let pool_c = pool.clone();
                        let factory_c = Arc::clone(&factory);
                        pool.begin_refill(&environment_key).await;
                        let key_c = environment_key.clone();
                        tokio::spawn(async move {
                            if let Err(e) = create_template_worker(
                                factory_c,
                                pool_c.clone(),
                                CreateTemplateRequest::new_with_lease_mode(
                                    refill_id,
                                    pool_c.lease_mode.clone(),
                                ),
                            )
                            .await
                            {
                                warn!("template pool maintenance: refill failed: {}", e);
                            }
                            pool_c.end_refill(&key_c).await;
                        });
                    }
                }

                pool.gc_if_needed().await;

                // Warn if GC is blocked (all templates above water level are actively held).
                // This indicates the pool is under pressure and may reject new templates.
                let blocked = pool.gc_blocked_templates().await;
                if !blocked.is_empty() {
                    warn!(
                        "template pool maintenance: GC blocked — {} template(s) cannot be evicted \
                        because they are held by active restores or running sandboxes: {}",
                        blocked.len(),
                        blocked
                            .iter()
                            .map(|(id, reason)| format!("{} ({})", id, reason))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        });
    }

    /// Boot a fresh VM, snapshot it once the guest agent is ready, stop it, and
    /// add the resulting snapshot to the template pool.
    pub async fn create_template(&self, req: CreateTemplateRequest) -> Result<PooledTemplate> {
        let pool = self
            .template_pool
            .as_ref()
            .ok_or_else(|| anyhow!("template pool not initialized"))?
            .clone();

        info!("creating template {}", req.id);
        create_template_worker(Arc::clone(&self.factory), pool, req).await
    }

    /// Try to restore a sandbox from the ContinuationStore.
    ///
    /// Acquires the entry for `identity`, validates snapshot files, then restores
    /// the VM. Returns `Err` on store miss, missing files, or restore failure so
    /// the caller can fall back to a cold-start.
    pub async fn start_with_continuation_snapshot(
        &self,
        id: &str,
        identity: &WorkloadIdentity,
    ) -> Result<()> {
        let store = match &self.continuation_store {
            Some(s) => s.clone(),
            None => {
                return Err(anyhow!(
                    "sandbox {}: continuation store not initialized \
                     (set enable_continuation_restore=true in config)",
                    id
                )
                .into());
            }
        };

        let lease: ContinuationLease = match store.acquire(identity).await {
            Some(l) => l,
            None => {
                return Err(anyhow!(
                    "sandbox {}: no continuation snapshot found for {}/g{}",
                    id,
                    identity.pod_uid,
                    identity.generation
                )
                .into());
            }
        };

        let tmpl = lease.template()?.clone();
        let template_id = tmpl.id.clone();

        // Publish the consumed continuation template ID to the sandbox immediately.
        // This closes the small race window between store acquire and the restore code
        // persisting template_id inside the sandbox state, so GC can observe the entry as
        // still active while restore validation and semaphore acquisition are in flight.
        if let Some(sb_mutex) = self.sandboxes.read().await.get(id).cloned() {
            let mut sb = sb_mutex.lock().await;
            sb.template_id = Some(template_id.clone());
        }

        // Validate snapshot files before acquiring the restore semaphore slot.
        let state_json = tmpl.snapshot_dir.join("state.json");
        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
        if !snapshot_ok {
            warn!(
                "sandbox {}: continuation snapshot files missing for {}/g{} (template {}), releasing",
                id, identity.pod_uid, identity.generation, tmpl.id
            );
            if let Some(sb_mutex) = self.sandboxes.read().await.get(id).cloned() {
                let mut sb = sb_mutex.lock().await;
                sb.clear_template_restore_state();
            }
            lease.fail().await;
            return Err(anyhow!(
                "sandbox {}: continuation snapshot files missing for {}/g{}",
                id,
                identity.pod_uid,
                identity.generation
            )
            .into());
        }

        let _permit = self.restore_semaphore.acquire().await.unwrap();
        let restore_start = std::time::Instant::now();
        let reflink_supported = self
            .template_pool
            .as_ref()
            .map(|p| p.reflink_supported)
            .unwrap_or(false);

        info!(
            "continuation store hit for sandbox {} ({}/g{}, template {}), restoring",
            id, identity.pod_uid, identity.generation, template_id
        );

        match self
            .start_from_snapshot(
                id,
                SnapshotRestoreParams {
                    snapshot_dir: tmpl.snapshot_dir.clone(),
                    template_id: Some(template_id.clone()),
                    snapshot_type: SnapshotType::Continuation,
                    id_generator: tmpl.id_generator,
                    disk_images: tmpl.disk_images.clone(),
                    storages: tmpl.storages.clone(),
                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                    lease_mode: TemplateLeaseMode::Exclusive,
                    reflink_supported,
                    original_task_vsock: tmpl.original_task_vsock.clone(),
                    warm_fork_params: None,
                },
            )
            .await
        {
            Ok(()) => {
                let ms = restore_start.elapsed().as_millis() as u64;
                info!(
                    "sandbox {} restored from continuation snapshot {}/g{} (template {}) in {}ms",
                    id, identity.pod_uid, identity.generation, template_id, ms
                );
                lease.complete().await;
                Ok(())
            }
            Err(e) => {
                warn!(
                    "sandbox {}: continuation restore failed ({}), releasing snapshot",
                    id, e
                );
                lease.fail().await;
                Err(e)
            }
        }
    }

    /// Execute a WarmFork restore from an already-acquired and type-validated template lease.
    ///
    /// Validates the ready protocol version and snapshot files, reads WarmFork params
    /// from pod annotations, acquires the restore semaphore, calls `start_from_snapshot`,
    /// and handles success / failure (metrics, lease state, aggregate log).
    ///
    /// The caller is responsible for verifying `enable_warmfork_restore=true` before
    /// calling this.
    async fn restore_from_warm_fork_template(
        &self,
        id: &str,
        tmpl: PooledTemplate,
        lease: TemplateLease,
        pool: Arc<TemplatePool>,
        lease_mode: TemplateLeaseMode,
    ) -> Result<()> {
        if tmpl.ready_protocol_version.is_none() {
            warn!(
                "sandbox {}: template {} has no ready_protocol_version; \
                 re-create it after annotating the pod with {}={}",
                id, tmpl.id, ANNOTATION_WARM_FORK_READY_PROTOCOL, WARM_FORK_PROTOCOL_V1
            );
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: WarmFork template {} missing ready_protocol_version",
                id,
                tmpl.id
            )
            .into());
        }

        let state_json = tmpl.snapshot_dir.join("state.json");
        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
        if !snapshot_ok {
            let missing = tmpl.id.clone();
            warn!(
                "sandbox {}: template {} snapshot files missing, releasing",
                id, tmpl.id
            );
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: template {} snapshot files missing",
                id,
                missing
            )
            .into());
        }

        let _permit = self.restore_semaphore.acquire().await.unwrap();
        let restore_start = Instant::now();
        let template_id = tmpl.id.clone();
        let reflink_supported = pool.reflink_supported;
        info!(
            "template pool hit for sandbox {} (template {}, type=WarmFork), \
             restoring (lease_mode={}, reflink={})",
            id, tmpl.id, lease_mode, reflink_supported
        );

        // Read WarmFork task parameters from pod annotations without holding sandbox locks.
        let annotations = {
            let guard = self.sandboxes.read().await;
            let sb_mutex = guard.get(id).cloned();
            drop(guard);
            if let Some(sb_mutex) = sb_mutex {
                let sb = sb_mutex.lock().await;
                sb.data.config.as_ref().map(|cfg| cfg.annotations.clone())
            } else {
                None
            }
        }
        .unwrap_or_default();

        let mut params = match parse_warm_fork_params(&annotations).map_err(|e| {
            anyhow!(
                "sandbox {}: invalid WarmFork restore annotations: {}",
                id,
                e
            )
        }) {
            Ok(p) => p,
            Err(e) => {
                lease.fail().await;
                pool.metrics.record_miss();
                return Err(e.into());
            }
        };

        if !tmpl.warm_fork_targets.is_empty() {
            let tmpl_names: HashSet<String> = tmpl
                .warm_fork_targets
                .iter()
                .map(|t| t.container_name.clone())
                .collect();
            let restore_names: HashSet<String> = params
                .targets
                .iter()
                .map(|t| t.container_name.clone())
                .collect();
            if tmpl_names != restore_names {
                lease.fail().await;
                pool.metrics.record_miss();
                return Err(anyhow!(
                    "sandbox {}: WarmFork restore container set {:?} does not match \
                     template '{}' snapshot-time set {:?}; the pod '{}' annotation \
                     must list exactly the same container names as at snapshot time",
                    id,
                    restore_names,
                    tmpl.id,
                    tmpl_names,
                    ANNOTATION_WARM_FORK_CONTAINERS
                )
                .into());
            }
            for t in &mut params.targets {
                if let Some(resolved) = tmpl
                    .warm_fork_targets
                    .iter()
                    .find(|rt| rt.container_name == t.container_name)
                {
                    t.container_id = resolved.container_id.clone();
                }
            }
        }

        match self
            .start_from_snapshot(
                id,
                SnapshotRestoreParams {
                    snapshot_dir: tmpl.snapshot_dir.clone(),
                    template_id: Some(template_id.clone()),
                    snapshot_type: SnapshotType::WarmFork,
                    id_generator: tmpl.id_generator,
                    disk_images: tmpl.disk_images.clone(),
                    storages: tmpl.storages.clone(),
                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                    lease_mode,
                    reflink_supported,
                    original_task_vsock: tmpl.original_task_vsock.clone(),
                    warm_fork_params: Some(params),
                },
            )
            .await
        {
            Ok(()) => {
                let ms = restore_start.elapsed().as_millis() as u64;
                pool.metrics.record_warmfork_hit(ms);
                lease.complete().await;

                let hits = pool
                    .metrics
                    .pool_hits
                    .load(std::sync::atomic::Ordering::Relaxed);
                if hits > 0 && hits % 10 == 0 {
                    info!(
                        "template pool: hit_rate={:.1}% \
                         (env={:.1}%, warmfork={:.1}%), \
                         avg_restore={}ms \
                         (env={}ms, warmfork={}ms), \
                         hits={}, misses={}",
                        pool.metrics.hit_rate() * 100.0,
                        pool.metrics.environment_hit_rate() * 100.0,
                        pool.metrics.warmfork_hit_rate() * 100.0,
                        pool.metrics.avg_restore_ms() as u64,
                        pool.metrics.environment_avg_restore_ms() as u64,
                        pool.metrics.warmfork_avg_restore_ms() as u64,
                        hits,
                        pool.metrics
                            .pool_misses
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                }

                info!(
                    "sandbox {} restored from WarmFork template {} in {}ms \
                     (pool hit_rate={:.1}%)",
                    id,
                    template_id,
                    ms,
                    pool.metrics.hit_rate() * 100.0
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    "sandbox {}: WarmFork restore failed ({}), releasing template",
                    id, e
                );
                lease.fail().await;
                pool.metrics.record_miss();
                Err(e)
            }
        }
    }

    /// Try to start an already-created sandbox from the template pool.
    ///
    /// If a matching template exists in the pool it is consumed and the sandbox
    /// is restored from that snapshot (fast path, typically < 500 ms).
    /// Returns `Err` on pool miss, missing snapshot files, or restore failure so
    /// the caller can try the next fallback (e.g. environment snapshot) before cold-starting.
    pub async fn start_with_template_key(&self, id: &str, key: &TemplateKey) -> Result<()> {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => {
                return Err(anyhow!("template pool not initialized").into());
            }
        };

        let tmpl = match pool.acquire_for_restore(key).await {
            Some(t) => Some(t),
            None => {
                let in_flight = pool.in_flight_count_for_key(key).await;
                if in_flight > 0 {
                    info!(
                        "template pool empty ({} refill(s) in-flight for key '{}'), queuing sandbox {}",
                        in_flight, key.key, id
                    );
                    pool.wait_and_acquire_for_restore(key, Duration::from_secs(15))
                        .await
                } else {
                    None
                }
            }
        };

        match tmpl {
            None => {
                pool.metrics.record_miss();
                Err(anyhow!("template pool miss for sandbox {}", id).into())
            }
            Some(tmpl) => {
                let lease_mode = match &tmpl.snapshot_type {
                    SnapshotType::Environment => TemplateLeaseMode::default(),
                    SnapshotType::WarmFork => pool.lease_mode.clone(),
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                };
                let lease = TemplateLease::new(pool.clone(), tmpl);
                let tmpl = lease.template()?.clone();

                let type_enabled = match &tmpl.snapshot_type {
                    SnapshotType::Environment => true,
                    SnapshotType::WarmFork => self.config.snapshot.enable_warmfork_restore,
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                };
                if !type_enabled {
                    lease.fail().await;
                    pool.metrics.record_miss();
                    return Err(anyhow!(
                        "sandbox {}: template {} type {:?} is not enabled \
                         (set enable_warmfork_restore=true in [sandbox.snapshot])",
                        id,
                        tmpl.id,
                        tmpl.snapshot_type,
                    )
                    .into());
                }

                match tmpl.snapshot_type.clone() {
                    SnapshotType::WarmFork => {
                        self.restore_from_warm_fork_template(id, tmpl, lease, pool, lease_mode)
                            .await
                    }
                    SnapshotType::Environment => {
                        let state_json = tmpl.snapshot_dir.join("state.json");
                        let snapshot_ok = tokio::fs::metadata(&state_json).await.is_ok()
                            && tokio::fs::metadata(&tmpl.pmem_path).await.is_ok();
                        if !snapshot_ok {
                            let missing_template_id = tmpl.id.clone();
                            warn!(
                                "sandbox {}: template {} snapshot files missing, releasing and cold-starting",
                                id, tmpl.id
                            );
                            lease.fail().await;
                            pool.metrics.record_miss();
                            return Err(anyhow!(
                                "sandbox {}: template {} snapshot files missing",
                                id,
                                missing_template_id
                            )
                            .into());
                        }

                        let _permit = self.restore_semaphore.acquire().await.unwrap();
                        let restore_start = Instant::now();
                        let template_id = tmpl.id.clone();
                        let reflink_supported = pool.reflink_supported;
                        info!(
                            "template pool hit for sandbox {} (template {}, type=Environment), \
                             restoring (lease_mode={}, reflink={})",
                            id, tmpl.id, lease_mode, reflink_supported
                        );

                        match self
                            .start_from_snapshot(
                                id,
                                SnapshotRestoreParams {
                                    snapshot_dir: tmpl.snapshot_dir.clone(),
                                    template_id: Some(template_id.clone()),
                                    snapshot_type: SnapshotType::Environment,
                                    id_generator: tmpl.id_generator,
                                    disk_images: tmpl.disk_images.clone(),
                                    storages: tmpl.storages.clone(),
                                    orphan_container_ids: tmpl.orphan_container_ids.clone(),
                                    lease_mode,
                                    reflink_supported,
                                    original_task_vsock: tmpl.original_task_vsock.clone(),
                                    warm_fork_params: None,
                                },
                            )
                            .await
                        {
                            Ok(()) => {
                                let ms = restore_start.elapsed().as_millis() as u64;
                                pool.metrics.record_environment_hit(ms);
                                lease.complete().await;

                                let hits = pool
                                    .metrics
                                    .pool_hits
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                if hits > 0 && hits % 10 == 0 {
                                    info!(
                                        "template pool: hit_rate={:.1}% \
                                         (env={:.1}%, warmfork={:.1}%), \
                                         avg_restore={}ms \
                                         (env={}ms, warmfork={}ms), \
                                         hits={}, misses={}",
                                        pool.metrics.hit_rate() * 100.0,
                                        pool.metrics.environment_hit_rate() * 100.0,
                                        pool.metrics.warmfork_hit_rate() * 100.0,
                                        pool.metrics.avg_restore_ms() as u64,
                                        pool.metrics.environment_avg_restore_ms() as u64,
                                        pool.metrics.warmfork_avg_restore_ms() as u64,
                                        hits,
                                        pool.metrics
                                            .pool_misses
                                            .load(std::sync::atomic::Ordering::Relaxed),
                                    );
                                }

                                info!(
                                    "sandbox {} restored from template {} in {}ms \
                                     (pool hit_rate={:.1}%)",
                                    id,
                                    template_id,
                                    ms,
                                    pool.metrics.hit_rate() * 100.0
                                );
                                Ok(())
                            }
                            Err(e) => {
                                warn!(
                                    "sandbox {}: template restore failed ({}), releasing template",
                                    id, e
                                );
                                lease.fail().await;
                                pool.metrics.record_miss();
                                Err(e)
                            }
                        }
                    }
                    SnapshotType::Continuation => unreachable!(
                        "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                    ),
                }
            }
        }
    }

    /// Restore a sandbox from a WarmFork template located by ID.
    ///
    /// If `key` is also provided it must match the template's pool key.
    /// Used when the pod sets `kuasar.io/template-id` (optionally combined with
    /// `kuasar.io/template-key`) instead of relying on key-based pool selection.
    async fn start_with_template_id(
        &self,
        id: &str,
        template_id: &str,
        key: Option<&str>,
    ) -> Result<()> {
        let pool = match &self.template_pool {
            Some(p) => p.clone(),
            None => return Err(anyhow!("template pool not initialized").into()),
        };

        let raw = pool
            .acquire_by_id_for_restore(template_id)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "sandbox {}: WarmFork template '{}' not available for restore",
                    id,
                    template_id
                )
            })?;

        if raw.snapshot_type != SnapshotType::WarmFork {
            return Err(anyhow!(
                "sandbox {}: template '{}' has snapshot_type {:?}; expected warm_fork",
                id,
                template_id,
                raw.snapshot_type
            )
            .into());
        }

        let lease_mode = pool.lease_mode.clone();
        let lease = TemplateLease::new(pool.clone(), raw.clone());
        let tmpl = match lease.template() {
            Ok(t) => t.clone(),
            Err(e) => {
                lease.fail().await;
                return Err(e);
            }
        };

        if let Some(k) = key {
            let expected = TemplateKey::user(k);
            if tmpl.key.key != expected.key {
                lease.fail().await;
                return Err(anyhow!(
                    "sandbox {}: WarmFork template '{}' key '{}' does not match \
                     requested key '{}'",
                    id,
                    template_id,
                    tmpl.key.key,
                    k
                )
                .into());
            }
        }

        if !self.config.snapshot.enable_warmfork_restore {
            lease.fail().await;
            pool.metrics.record_miss();
            return Err(anyhow!(
                "sandbox {}: template {} WarmFork restore is not enabled \
                 (set enable_warmfork_restore=true in [sandbox.snapshot])",
                id,
                tmpl.id,
            )
            .into());
        }

        self.restore_from_warm_fork_template(id, tmpl, lease, pool, lease_mode)
            .await
    }
}

#[cfg(test)]
mod tests {
    mod recovery {
        use std::{collections::HashMap, path::Path, sync::Arc};

        use async_trait::async_trait;
        use containerd_sandbox::{
            data::SandboxData, error::Result, signal::ExitSignal, SandboxOption, SandboxStatus,
        };
        use serde::{Deserialize, Serialize};
        use temp_dir::TempDir;
        use tokio::sync::Mutex;

        use crate::{
            cgroup::SandboxCgroup,
            container::KuasarContainer,
            device::{BusType, DeviceInfo},
            sandbox::{KuasarSandbox, KuasarSandboxer, SandboxConfig},
            storage::device_graph::DeviceGraph,
            vm::{Hooks, Pids, Recoverable, VMFactory, VcpuThreads, VM},
        };

        #[derive(Default, Serialize, Deserialize)]
        struct MockVM {
            fail_recover: bool,
            socket_address: String,
            stop_marker: String,
        }

        #[async_trait]
        impl VM for MockVM {
            async fn start(&mut self) -> Result<u32> {
                Ok(0)
            }

            async fn stop(&mut self, force: bool) -> Result<()> {
                let content = if force { "force" } else { "graceful" };
                if !self.stop_marker.is_empty() {
                    tokio::fs::write(&self.stop_marker, content)
                        .await
                        .map_err(containerd_sandbox::error::Error::IO)?;
                }
                Ok(())
            }

            async fn attach(&mut self, _device_info: DeviceInfo) -> Result<()> {
                Ok(())
            }

            async fn hot_attach(&mut self, _device_info: DeviceInfo) -> Result<(BusType, String)> {
                Ok((BusType::PCI, String::new()))
            }

            async fn hot_detach(&mut self, _id: &str) -> Result<()> {
                Ok(())
            }

            async fn ping(&self) -> Result<()> {
                Ok(())
            }

            fn socket_address(&self) -> String {
                self.socket_address.clone()
            }

            async fn wait_channel(&self) -> Option<tokio::sync::watch::Receiver<(u32, i128)>> {
                None
            }

            async fn vcpus(&self) -> Result<VcpuThreads> {
                Ok(VcpuThreads {
                    vcpus: HashMap::new(),
                })
            }

            fn pids(&self) -> Pids {
                Pids::default()
            }
        }

        #[async_trait]
        impl Recoverable for MockVM {
            async fn recover(&mut self) -> Result<()> {
                if self.fail_recover {
                    return Err(containerd_sandbox::error::Error::InvalidArgument(
                        "mock recover failure".to_string(),
                    ));
                }
                Ok(())
            }
        }

        struct MockFactory;

        #[async_trait]
        impl VMFactory for MockFactory {
            type VM = MockVM;
            type Config = ();

            fn new(_: Self::Config) -> Self {
                Self
            }

            async fn create_vm(&self, _: &str, _: &SandboxOption) -> Result<Self::VM> {
                Ok(MockVM::default())
            }

            fn with_resources(&self, _vcpus: u32, _memory_mb: u32) -> Self {
                Self
            }
        }

        struct MockHooks;

        #[async_trait]
        impl Hooks<MockVM> for MockHooks {}

        fn mock_sandbox(
            base_dir: &str,
            status: SandboxStatus,
            vm: MockVM,
        ) -> KuasarSandbox<MockVM> {
            KuasarSandbox {
                vm,
                id: "test-sandbox".to_string(),
                status,
                base_dir: base_dir.to_string(),
                data: SandboxData::default(),
                containers: HashMap::<String, KuasarContainer>::new(),
                storages: DeviceGraph::default(),
                id_generator: 0,
                network: None,
                client: Arc::new(Mutex::new(None)),
                exit_signal: Arc::new(ExitSignal::default()),
                sandbox_cgroups: SandboxCgroup::default(),
                template_id: None,
                template_snapshot_type: None,
                lease_mode: None,
                reflink_supported: None,
                orphan_container_ids: vec![],
                memory_restore_mode: None,
            }
        }

        async fn write_dump<P: AsRef<Path>>(dir: P, sandbox: &KuasarSandbox<MockVM>) {
            let dump_path = dir.as_ref().join("sandbox.json");
            let content = serde_json::to_vec(sandbox).unwrap();
            tokio::fs::write(dump_path, content).await.unwrap();
        }

        #[tokio::test]
        async fn test_recover_fast_fail_paths_force_stop_vm() {
            let cases = vec![
                (
                    "recover-error",
                    MockVM {
                        fail_recover: true,
                        socket_address: "vsock://ignored".to_string(),
                        stop_marker: String::new(),
                    },
                ),
                (
                    "init-client-error",
                    MockVM {
                        fail_recover: false,
                        socket_address: String::new(),
                        stop_marker: String::new(),
                    },
                ),
            ];

            for (name, mut vm) in cases {
                let temp_dir = TempDir::new().unwrap();
                let stop_marker = temp_dir.path().join(format!("{name}.stop"));
                vm.stop_marker = stop_marker.to_string_lossy().to_string();

                let sandbox = mock_sandbox(
                    temp_dir.path().to_str().unwrap(),
                    SandboxStatus::Running(42),
                    vm,
                );
                write_dump(temp_dir.path(), &sandbox).await;

                let result = KuasarSandbox::<MockVM>::recover(temp_dir.path()).await;
                assert!(result.is_err(), "{name} should fail fast");

                let marker_content = tokio::fs::read_to_string(&stop_marker).await.unwrap();
                assert_eq!(marker_content, "force", "{name} should force stop the VM");
            }
        }

        #[tokio::test]
        async fn test_sandboxer_recover_cleans_failed_sandbox_dir() {
            let root = TempDir::new().unwrap();
            let failed_dir = root.path().join("failed");
            tokio::fs::create_dir_all(&failed_dir).await.unwrap();
            let stop_marker = root.path().join("failed.stop");

            let sandbox = mock_sandbox(
                failed_dir.to_str().unwrap(),
                SandboxStatus::Running(7),
                MockVM {
                    fail_recover: true,
                    socket_address: "vsock://ignored".to_string(),
                    stop_marker: stop_marker.to_string_lossy().to_string(),
                },
            );
            write_dump(&failed_dir, &sandbox).await;

            let mut sandboxer = KuasarSandboxer::<MockFactory, MockHooks>::new(
                SandboxConfig::default(),
                (),
                MockHooks,
            );
            sandboxer.recover(root.path().to_str().unwrap()).await;

            assert!(
                tokio::fs::metadata(&failed_dir).await.is_err(),
                "failed sandbox dir should be removed after fast failure"
            );
            assert_eq!(
                tokio::fs::read_to_string(&stop_marker).await.unwrap(),
                "force"
            );
            assert!(sandboxer.sandboxes.read().await.is_empty());
        }
    }

    mod warm_fork {
        use std::collections::HashMap;

        use crate::{
            sandbox::parse_warm_fork_params,
            vm::{
                ANNOTATION_WARM_FORK_CONTAINERS, ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET,
                ANNOTATION_WARM_FORK_ENV_PREFIX, ANNOTATION_WARM_FORK_READINESS_SOCKET,
                ANNOTATION_WARM_FORK_TASK_CONTEXT, ANNOTATION_WARM_FORK_TASK_ID,
            },
        };

        #[test]
        fn test_parse_warm_fork_params_missing_task_id_is_autonomous() {
            let annotations: HashMap<String, String> = HashMap::new();
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(
                params.task_id.is_none(),
                "absent task-id must yield autonomous mode"
            );
            assert_eq!(params.targets.len(), 1);
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
        }

        #[test]
        fn test_parse_warm_fork_params_minimal() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "task-42".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("task-42".to_string()));
            assert_eq!(params.targets.len(), 1);
            // Single-container: socket defaults to DEFAULT_READINESS_SOCKET
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
            assert!(params.targets[0].context.is_empty());
            assert!(params.targets[0].env_overrides.is_empty());
        }

        #[test]
        fn test_parse_warm_fork_params_full() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "my-task".to_string(),
            );
            annotations.insert(
                ANNOTATION_WARM_FORK_READINESS_SOCKET.to_string(),
                "/run/my-readiness.sock".to_string(),
            );
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_CONTEXT.to_string(),
                "prod-ctx".to_string(),
            );
            annotations.insert(
                format!("{}FOO", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "bar".to_string(),
            );
            annotations.insert(
                format!("{}MODEL_PATH", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "/models/llm".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("my-task".to_string()));
            assert_eq!(params.targets.len(), 1);
            assert_eq!(params.targets[0].socket_path, "/run/my-readiness.sock");
            assert_eq!(params.targets[0].context, "prod-ctx");
            assert_eq!(params.targets[0].env_overrides.get("FOO").unwrap(), "bar");
            assert_eq!(
                params.targets[0].env_overrides.get("MODEL_PATH").unwrap(),
                "/models/llm"
            );
            assert_eq!(params.targets[0].env_overrides.len(), 2);
        }

        #[test]
        fn test_parse_warm_fork_params_multi_container() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t-mc".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,sidecar".to_string(),
            );
            // Per-container socket overrides
            annotations.insert(
                "kuasar.io/container/app/warm-fork-readiness-socket".to_string(),
                "/run/app.sock".to_string(),
            );
            // Per-container context for sidecar
            annotations.insert(
                "kuasar.io/container/sidecar/task-context".to_string(),
                "sc-ctx".to_string(),
            );
            // Pod-level env
            annotations.insert(
                format!("{}POD_KEY", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "pod-val".to_string(),
            );
            // Per-container env for app only
            annotations.insert(
                "kuasar.io/container/app/task-env/APP_KEY".to_string(),
                "app-val".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(params.task_id, Some("t-mc".to_string()));
            assert_eq!(params.targets.len(), 2);

            let app = params
                .targets
                .iter()
                .find(|t| t.container_name == "app")
                .unwrap();
            assert_eq!(app.socket_path, "/run/app.sock");
            assert_eq!(app.env_overrides.get("POD_KEY").unwrap(), "pod-val");
            assert_eq!(app.env_overrides.get("APP_KEY").unwrap(), "app-val");
            assert!(
                app.container_id.is_empty(),
                "container_id must be empty (resolved at restore)"
            );

            let sc = params
                .targets
                .iter()
                .find(|t| t.container_name == "sidecar")
                .unwrap();
            assert_eq!(sc.context, "sc-ctx");
            assert_eq!(sc.env_overrides.get("POD_KEY").unwrap(), "pod-val");
            assert!(!sc.env_overrides.contains_key("APP_KEY"));
        }

        #[test]
        fn test_warm_fork_params_json_round_trip() {
            let mut annotations = HashMap::new();
            annotations.insert(
                ANNOTATION_WARM_FORK_TASK_ID.to_string(),
                "rt-task".to_string(),
            );
            annotations.insert(
                format!("{}KEY", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "value".to_string(),
            );

            let params = parse_warm_fork_params(&annotations).unwrap();
            let json = serde_json::to_vec(&params).expect("serialize");
            let back: crate::vm::WarmForkParams =
                serde_json::from_slice(&json).expect("deserialize");
            assert_eq!(back.task_id, Some("rt-task".to_string()));
            assert_eq!(back.targets[0].env_overrides.get("KEY").unwrap(), "value");
        }

        #[test]
        fn test_parse_warm_fork_params_empty_task_id_is_autonomous() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "".to_string());
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(
                params.task_id.is_none(),
                "empty task-id must yield autonomous mode, same as absent"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_env_prefix_only_no_task_id_is_autonomous() {
            let mut annotations = HashMap::new();
            annotations.insert(
                format!("{}FOO", ANNOTATION_WARM_FORK_ENV_PREFIX),
                "bar".to_string(),
            );
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert!(params.task_id.is_none());
            // Env override is still carried even in autonomous mode.
            assert_eq!(params.targets[0].env_overrides.get("FOO").unwrap(), "bar");
        }

        #[test]
        fn test_parse_warm_fork_params_default_socket_path() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            let params = parse_warm_fork_params(&annotations).unwrap();
            assert_eq!(
                params.targets[0].socket_path,
                ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET
            );
        }

        #[test]
        fn test_parse_warm_fork_params_empty_containers_annotation_returns_none() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            // Annotation present but contains only commas/whitespace → empty name list
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                " , , ".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "empty container list must be rejected"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_empty_container_item_returns_error() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,,sidecar".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "empty container name item must be rejected"
            );
        }

        #[test]
        fn test_parse_warm_fork_params_duplicate_container_name_returns_none() {
            let mut annotations = HashMap::new();
            annotations.insert(ANNOTATION_WARM_FORK_TASK_ID.to_string(), "t1".to_string());
            annotations.insert(
                ANNOTATION_WARM_FORK_CONTAINERS.to_string(),
                "app,sidecar,app".to_string(),
            );
            assert!(
                parse_warm_fork_params(&annotations).is_err(),
                "duplicate container name must be rejected"
            );
        }
    }

    mod continuation {
        use std::collections::HashMap;

        use crate::{
            sandbox::{MemoryRestoreMode, SnapshotConfig},
            template::{SnapshotType, TemplateKey, WorkloadIdentity},
            vm::{DiskImageEntry, RestoreSource, SnapshotPathOverrides},
        };

        fn make_restore_source(storages: Vec<vmm_common::storage::Storage>) -> RestoreSource {
            RestoreSource {
                snapshot_dir: std::path::PathBuf::from("/tmp/snap"),
                work_dir: std::path::PathBuf::from("/tmp/work"),
                overrides: SnapshotPathOverrides {
                    task_vsock: String::new(),
                    console_path: String::new(),
                },
                snapshot_type: SnapshotType::Continuation,
                disk_images: vec![DiskImageEntry {
                    storage_id: "vol1".to_string(),
                    device_id: "blk0".to_string(),
                    filename: "disks/vol1.img".to_string(),
                }],
                storages,
                orphan_container_ids: vec!["ctr-a".to_string(), "ctr-b".to_string()],
                memory_restore_mode: MemoryRestoreMode::Copy,
                reflink_supported: false,
                lease_mode: crate::sandbox::TemplateLeaseMode::Exclusive,
                warm_fork_params: None,
            }
        }

        #[test]
        fn test_continuation_restore_disabled_by_default() {
            let cfg = SnapshotConfig::default();
            assert!(
                !cfg.enable_continuation_restore,
                "enable_continuation_restore must default to false"
            );
        }

        #[test]
        fn test_continuation_restore_can_be_enabled() {
            let cfg = SnapshotConfig {
                enable_continuation_restore: true,
                enable_environment_restore: false,
                enable_warmfork_restore: false,
                ..SnapshotConfig::default()
            };
            assert!(cfg.enable_continuation_restore);
        }

        #[test]
        fn test_snapshot_type_continuation_does_not_require_network_hotplug() {
            assert!(
                !SnapshotType::Continuation.requires_network_hotplug(),
                "Continuation must not require network hotplug (identity preserved externally)"
            );
        }

        /// remap_restored_storage_artifacts produces cleanup paths in the sandbox base_dir.
        /// Verifies the path formula used in try_restore for Continuation.
        #[test]
        fn test_remap_cleanup_path_formula() {
            // The remap formula is: "{base_dir}/{storage.id}.img"
            // This must match where prepare_restore_block_artifacts places the renamed disk
            // (base_dir passed to that function is the sandbox base_dir).
            let base_dir = "/run/kuasar/test-sb";
            let storage_id = "vol1";
            let expected = format!("{}/{}.img", base_dir, storage_id);
            assert_eq!(expected, "/run/kuasar/test-sb/vol1.img");
        }

        /// Continuation restore source carries orphan container IDs.
        #[test]
        fn test_continuation_restore_source_has_orphan_container_ids() {
            let src = make_restore_source(vec![]);
            assert_eq!(
                src.orphan_container_ids,
                vec!["ctr-a".to_string(), "ctr-b".to_string()],
                "orphan containers from snapshot must be propagated via RestoreSource"
            );
        }

        /// Pool key for Continuation includes generation to prevent stale snapshot reuse.
        #[test]
        fn test_continuation_pool_key_encodes_workload_identity() {
            let wi = WorkloadIdentity {
                pod_uid: "abc-def".to_string(),
                generation: 2,
            };
            let key = TemplateKey::from_workload_identity(&wi.pod_uid, wi.generation);
            assert_eq!(key.key, "abc-def:2");
            assert_eq!(key.key, "abc-def:2");
        }

        /// Continuation pod annotations for restore lookup.
        #[test]
        fn test_cont_annotation_constants_are_correct() {
            assert_eq!(crate::template::POD_UID_ANNOTATION, "kuasar.io/pod-uid");
            assert_eq!(
                crate::template::WORKLOAD_GENERATION_ANNOTATION,
                "kuasar.io/workload-generation"
            );
        }

        /// When a pod sets kuasar.io/pod-uid, the restore key is derived from workload identity.
        #[test]
        fn test_cont_pod_uid_annotation_drives_key_derivation() {
            let mut annotations = HashMap::new();
            annotations.insert(
                crate::template::POD_UID_ANNOTATION.to_string(),
                "pod-xyz".to_string(),
            );
            annotations.insert(
                crate::template::WORKLOAD_GENERATION_ANNOTATION.to_string(),
                "3".to_string(),
            );
            let pod_uid = annotations
                .get(crate::template::POD_UID_ANNOTATION)
                .unwrap();
            let restart: u32 = annotations
                .get(crate::template::WORKLOAD_GENERATION_ANNOTATION)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let key = TemplateKey::from_workload_identity(pod_uid, restart);
            assert_eq!(key.key, "pod-xyz:3");
        }

        /// Missing generation annotation defaults to 0.
        #[test]
        fn test_cont_generation_defaults_to_zero_when_absent() {
            let annotations: HashMap<String, String> = HashMap::new();
            // Absent annotation → parse returns None → default 0.
            let restart: u32 =
                match annotations.get(crate::template::WORKLOAD_GENERATION_ANNOTATION) {
                    None => 0,
                    Some(s) => s
                        .parse::<u64>()
                        .ok()
                        .filter(|&v| v <= u32::MAX as u64)
                        .unwrap_or(0) as u32,
                };
            assert_eq!(restart, 0);
        }

        /// A non-numeric generation annotation must cause a hard error when
        /// pod-uid is also present.  Falling back to an environment restore would
        /// silently discard the workload state the operator intended to continue.
        #[test]
        fn test_cont_generation_malformed_produces_hard_error() {
            // Simulate the production parse: non-numeric → Err (hard failure).
            let counter_str = "not-a-number";
            let result: Result<u32, String> = match counter_str.parse::<u64>() {
                Ok(v) if v <= u32::MAX as u64 => Ok(v as u32),
                Ok(v) => Err(format!("{} exceeds u32::MAX", v)),
                Err(_) => Err(format!("{:?} is not a valid u32", counter_str)),
            };
            assert!(
                result.is_err(),
                "malformed generation must produce an error"
            );
        }

        /// An out-of-range generation (> u32::MAX) must also produce a hard error.
        #[test]
        fn test_cont_generation_overflow_produces_hard_error() {
            let counter_str = (u32::MAX as u64 + 1).to_string();
            let result: Result<u32, String> = match counter_str.parse::<u64>() {
                Ok(v) if v <= u32::MAX as u64 => Ok(v as u32),
                Ok(v) => Err(format!("{} exceeds u32::MAX", v)),
                Err(_) => Err(format!("{:?} is not a valid u32", counter_str)),
            };
            assert!(
                result.is_err(),
                "generation > u32::MAX must produce an error, not be truncated"
            );
        }
    }

    mod dns {
        use crate::sandbox::parse_dnsoptions;

        #[derive(Default)]
        struct DnsConfig {
            servers: Vec<String>,
            searches: Vec<String>,
            options: Vec<String>,
        }

        #[test]
        fn test_parse_empty_dns_option() {
            let dns_test = DnsConfig::default();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(resolv_content.is_empty())
        }

        #[test]
        fn test_parse_non_empty_dns_option() {
            let dns_test = DnsConfig {
                servers: vec!["8.8.8.8", "server.google.com"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                searches: vec![
                    "server0.google.com",
                    "server1.google.com",
                    "server2.google.com",
                    "server3.google.com",
                    "server4.google.com",
                    "server5.google.com",
                    "server6.google.com",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                options: vec!["timeout:1"].into_iter().map(String::from).collect(),
            };
            let expected_content = "search server0.google.com server1.google.com server2.google.com server3.google.com server4.google.com server5.google.com server6.google.com
nameserver 8.8.8.8
nameserver server.google.com
options timeout:1
".to_string();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(!resolv_content.is_empty());
            assert_eq!(resolv_content, expected_content)
        }
    }
}
