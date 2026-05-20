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
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use serde::{Deserialize, Serialize};
use vmm_common::storage::Storage;

use crate::{utils::write_file_atomic, vm::DiskImageEntry};

/// Snapshot type declared immutably at creation time.
/// Determines snapshot content, pool sharing semantics, and valid restore operations.
///
/// `SnapshotType` and `memory_restore_mode` are orthogonal dimensions:
/// - `SnapshotType` — business semantics: what was captured, how the pool manages the entry,
///   and what post-restore steps are required (network hotplug, task injection, etc.).
/// - `memory_restore_mode` — infrastructure: how guest memory is loaded at restore time
///   (Copy, OnDemand, FileBackend, ExternalUffd). All four modes are valid for all three
///   snapshot types. "Exclusive consume" for Continuation means the pool entry cannot be
///   reused, but the snapshot files remain on disk until the sandbox is stopped and deleted,
///   so FileBackend and ExternalUffd backing stores are fully safe for Continuation.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotType {
    /// No containers at snapshot time. Freely shareable (shared lease).
    /// Requires network device hotplug after restore.
    #[default]
    Environment,
    /// Ready-waiting process. Shared template lease; each instance has independent guest memory.
    /// Requires task injection via guest agent before process resumes.
    /// Physical CoW page sharing across instances (via FileBackend / ExternalUffd modes)
    /// is deferred to a future phase.
    WarmFork,
    /// Full process state + network identity. Exclusive (1:1 consume). Not pooled.
    /// Compatible with all four memory restore modes. "Exclusive consume" means the pool entry
    /// cannot be reused, not that snapshot files are deleted; files persist until sandbox delete.
    Continuation,
}

impl<'de> serde::Deserialize<'de> for SnapshotType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "environment" => Ok(SnapshotType::Environment),
            "warm_fork" => Ok(SnapshotType::WarmFork),
            "continuation" => Ok(SnapshotType::Continuation),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["environment", "warm_fork", "continuation"],
            )),
        }
    }
}

impl SnapshotType {
    /// Parse a snapshot-type annotation value.
    pub fn from_annotation(value: &str) -> anyhow::Result<Self> {
        match value {
            "warm-fork" | "warm_fork" => Ok(SnapshotType::WarmFork),
            "continuation" => Ok(SnapshotType::Continuation),
            _ => Err(anyhow!(
                "unknown snapshot type annotation '{}'; environment is the default, omit this annotation instead of setting it explicitly",
                value
            )),
        }
    }

    /// Whether restored instances get freshly hotplugged network namespaces
    /// (as opposed to preserving the snapshot's original network identity).
    pub fn requires_network_hotplug(&self) -> bool {
        matches!(self, SnapshotType::Environment | SnapshotType::WarmFork)
    }
}

/// Uniquely identifies a specific running workload instance for `ContinuationSnapshot`.
/// Enforces singleton restore semantics: only one restore per (pod_uid, generation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WorkloadIdentity {
    pub pod_uid: String,
    pub generation: u32,
}

/// Per-container metadata captured at snapshot time, used for cross-sandbox
/// storage deduplication on restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotContainerMeta {
    pub id: String,
    /// The lowerdir= value from the container's overlay mount options.
    pub lower_dirs: String,
    pub storage_id: String,
    pub device_id: String,
    /// Absolute path to the ext4 backing image on the host at snapshot time.
    pub img_path: String,
}

/// Identifies a class of templates that can be interchangeably restored into a sandbox.
///
/// Two construction paths:
/// - **VM snapshots**: `TemplateKey::from_vm_config(kernel, image, vcpus, memory, kernel_params, storage_backend)` — the
///   sandboxer derives the key automatically; callers do not set it.
/// - **WarmFork/user snapshots**: `TemplateKey::user(key)` — the operator supplies an opaque
///   string via the `kuasar.io/template-key` pod annotation or the admin API `key` field.
///   Correctness (i.e. that two pods with the same key can safely share a snapshot) is
///   the caller's responsibility.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct TemplateKey {
    pub key: String,
}

impl TemplateKey {
    /// Build a key for a bare-VM template from the VM's actual configuration.
    ///
    /// All parameters that affect the guest environment are included so that
    /// templates built from different configurations never collide:
    /// - `kernel_path` / `image_path`: different kernels or root filesystems are incompatible.
    /// - `vcpus` / `memory_mb`: resource dimensions must match.
    /// - `kernel_params`: user-supplied cmdline additions change guest behaviour.
    /// - `storage_backend`: virtio-blk and virtiofs guests use different task binary paths.
    ///
    /// NOTE: the guest task binary version is part of the intended Environment key, but it is
    /// not yet available from the current factory inputs. Environment snapshot creation should
    /// fail closed until the sandboxer can reliably read that version from config or a build-
    /// time constant embedded in the guest image.
    pub fn from_vm_config(
        kernel_path: &str,
        image_path: &str,
        vcpus: u32,
        memory_mb: u32,
        kernel_params: &str,
        storage_backend: &str,
    ) -> Self {
        Self {
            key: format!(
                "{}:{}:{}:{}:{}:{}",
                kernel_path, image_path, vcpus, memory_mb, kernel_params, storage_backend
            ),
        }
    }

    /// Build a key from a user-supplied opaque string (WarmFork pool key).
    pub fn user(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }

    /// Build the canonical key for a `ContinuationSnapshot`.
    ///
    /// The key uniquely identifies a specific running workload instance.  On the restore side,
    /// the sandboxer derives this key from the pod annotations `kuasar.io/pod-uid` and
    /// `kuasar.io/workload-generation`.  On the create side, callers supply `pod_uid` and
    /// `generation` via the admin API (`snapshot_type=continuation`).
    ///
    /// Format: `"{pod_uid}:{generation}"`
    pub fn from_workload_identity(pod_uid: &str, generation: u32) -> Self {
        Self {
            key: format!("{}:{}", pod_uid, generation),
        }
    }
}

/// A single pre-warmed VM snapshot ready to be restored into a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PooledTemplate {
    pub id: String,
    pub key: TemplateKey,
    /// Directory containing CH snapshot files (memory-ranges/, state.json, config.json).
    pub snapshot_dir: PathBuf,
    pub pmem_path: String,
    pub kernel_path: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    /// Unix timestamp (seconds) when this template was created.
    pub created_at_secs: u64,
    /// Original hvsock path recorded at snapshot time (needed to compute overrides).
    pub original_task_vsock: String,
    /// Original console log path recorded at snapshot time.
    pub original_console_path: String,
    /// Cloud Hypervisor binary version string recorded at snapshot time.
    /// Used to detect cross-version restores that may hit incompatible API fields.
    /// Empty string means "unknown" (templates written before this field was added).
    #[serde(default)]
    pub hypervisor_api_version: String,
    /// Snapshot type declared at creation time. Drives all restore behavior.
    /// Serialized as "snapshot_type". Values: "environment", "warm_fork", "continuation".
    #[serde(default)]
    pub snapshot_type: SnapshotType,
    /// The id_generator value of the sandbox that was snapshotted.  Restored sandboxes must
    /// start their counter here so hotplugged device IDs (blk<N>) don't collide with device
    /// IDs already present in the restored VM state.
    #[serde(default)]
    pub id_generator: u32,
    /// Disk images captured in this snapshot (full-checkpoint mode).
    /// Empty for bare-VM templates; non-empty when snapshotted from a running sandbox with
    /// active virtio-blk container layers.  Restored sandboxes copy these files into their
    /// sandbox directory and CH remaps the backing paths via the patched config.json.
    #[serde(default)]
    pub disk_images: Vec<DiskImageEntry>,
    /// Storage entries from the snapshotted sandbox, used on WarmFork restore to
    /// rebuild the storage registry so new containers can reuse existing block devices.
    #[serde(default)]
    pub storages: Vec<Storage>,
    /// Container IDs that were running at snapshot time and will be orphaned on restore.
    #[serde(default)]
    pub orphan_container_ids: Vec<String>,
    /// Per-container snapshot metadata for cross-sandbox storage deduplication.
    #[serde(default)]
    pub snapshot_containers: Vec<SnapshotContainerMeta>,
    /// Ready-waiting protocol version; validated before WarmFork restore.
    /// None for Environment and Continuation types.
    #[serde(default)]
    pub ready_protocol_version: Option<String>,
    /// Workload identity for Continuation snapshots.
    /// None for Environment and WarmFork types.
    #[serde(default)]
    pub workload_identity: Option<WorkloadIdentity>,
    /// Snapshot-time resolved injection targets for WarmFork templates.
    /// Each entry carries the (container_name, CRI container_id, socket_path) tuple
    /// resolved against the running sandbox at snapshot time.  At restore time the
    /// container_id is overlaid onto the restore-pod's WarmForkParams so the guest
    /// agent can enter the correct mount namespace via setns.
    /// Empty for Environment and Continuation types.
    #[serde(default)]
    pub warm_fork_targets: Vec<crate::vm::WarmForkTarget>,
}

impl PooledTemplate {
    /// Persist to `{dir}/pooled_template.json` atomically.
    pub async fn save(&self, dir: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize PooledTemplate: {}", e))?;
        write_file_atomic(&dir.join("pooled_template.json"), &content).await
    }

    /// Load from `{dir}/pooled_template.json`.
    pub async fn load(dir: &Path) -> Result<Self> {
        let content = tokio::fs::read_to_string(dir.join("pooled_template.json"))
            .await
            .map_err(|e| anyhow!("read pooled_template.json from {}: {}", dir.display(), e))?;
        serde_json::from_str(&content)
            .map_err(|e| anyhow!("parse pooled_template.json: {}", e).into())
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        key: TemplateKey,
        snapshot_dir: PathBuf,
        pmem_path: impl Into<String>,
        kernel_path: impl Into<String>,
        vcpus: u32,
        memory_mb: u32,
        original_task_vsock: impl Into<String>,
        original_console_path: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            key,
            snapshot_dir,
            pmem_path: pmem_path.into(),
            kernel_path: kernel_path.into(),
            vcpus,
            memory_mb,
            created_at_secs: Self::now_secs(),
            original_task_vsock: original_task_vsock.into(),
            original_console_path: original_console_path.into(),
            hypervisor_api_version: String::new(),
            snapshot_type: SnapshotType::Environment,
            id_generator: 0,
            disk_images: vec![],
            storages: vec![],
            orphan_container_ids: vec![],
            snapshot_containers: vec![],
            ready_protocol_version: None,
            workload_identity: None,
            warm_fork_targets: vec![],
        }
    }
}

/// Request to create a pre-warmed VM template snapshot by booting a fresh VM.
pub struct CreateTemplateRequest {
    /// Unique identifier for this template entry.
    pub id: String,
    /// Template pool key.
    /// `None` → auto-compute from VM factory config (Environment templates).
    /// `Some(k)` → user-supplied opaque key (WarmFork templates).
    pub key: Option<String>,
    /// vCPU count override. `None` → use factory default.
    pub vcpus: Option<u32>,
    /// Memory in MiB override. `None` → use factory default.
    pub memory_mb: Option<u32>,
    /// Share mode for the snapshot (Shared = reflink COW, Exclusive = symlink).
    pub lease_mode: crate::sandbox::TemplateLeaseMode,
}

impl CreateTemplateRequest {
    /// Bare-VM template: key is auto-computed from factory config.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            key: None,
            vcpus: None,
            memory_mb: None,
            lease_mode: crate::sandbox::TemplateLeaseMode::default(),
        }
    }

    /// Bare-VM template with explicit share mode.
    pub fn new_with_lease_mode(
        id: impl Into<String>,
        lease_mode: crate::sandbox::TemplateLeaseMode,
    ) -> Self {
        Self {
            id: id.into(),
            key: None,
            vcpus: None,
            memory_mb: None,
            lease_mode,
        }
    }

    /// Container snapshot: key is supplied by the caller.
    pub fn with_key(id: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            key: Some(key.into()),
            vcpus: None,
            memory_mb: None,
            lease_mode: crate::sandbox::TemplateLeaseMode::default(),
        }
    }
}
