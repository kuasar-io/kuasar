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
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    error::{Error, Result},
    SandboxOption,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch::Receiver;
use ttrpc::context::with_timeout;
use vmm_common::api::{sandbox::CheckRequest, sandbox_ttrpc::SandboxServiceClient};

use crate::{
    client::new_sandbox_client_fail_fast,
    device::{BusType, DeviceInfo},
    sandbox::{KuasarSandbox, MemoryRestoreMode},
    template::SnapshotType,
};

const VIRTIO_FS: &str = "virtio-fs";
const VIRTIO_9P: &str = "virtio-9p";
pub const VIRTIO_BLK: &str = "virtio-blk";
pub const VIRTIOFS: &str = "virtiofs";

pub const DEFAULT_BLOCK_IMAGE_SIZE_OVERHEAD_PERCENT: u32 = 20;
pub const DEFAULT_SMALL_DIR_MAX_FILES: usize = 50;
pub const DEFAULT_SMALL_DIR_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_OVERLAY_IMAGE_FALLBACK_SIZE_MB: u64 = 64;
pub const DEFAULT_BIND_IMAGE_FALLBACK_SIZE_MB: u64 = 8;

#[async_trait]
pub trait VMFactory {
    type VM: VM + Sync + Send;
    type Config: Sync + Send;
    fn new(config: Self::Config) -> Self;
    async fn create_vm(&self, id: &str, s: &SandboxOption) -> Result<Self::VM>;

    // Optional accessors used by the template pool to build TemplateKey and PooledTemplate.
    // Implementations that support templating should override these.
    fn image_path(&self) -> &str {
        ""
    }
    fn kernel_path(&self) -> &str {
        ""
    }
    fn vcpus(&self) -> u32 {
        1
    }
    fn memory_mb(&self) -> u32 {
        1024
    }
    /// User-supplied kernel parameters (appended to the base cmdline).
    /// Included in the Environment TemplateKey so VMs with different boot parameters
    /// never share a snapshot.
    fn kernel_params(&self) -> &str {
        ""
    }
    /// Container storage backend identifier (e.g. "virtio-blk", "virtiofs").
    /// Included in the Environment TemplateKey so virtio-blk and virtiofs VMs never
    /// share a snapshot (their guest agent paths and task binary differ).
    fn storage_backend(&self) -> &str {
        ""
    }

    /// Return a new factory instance with overridden vcpus and memory for template creation.
    ///
    /// Override this in implementations that support per-template resource customisation.
    fn with_resources(&self, vcpus: u32, memory_mb: u32) -> Self;
}

#[async_trait]
pub trait Hooks<V: VM + Sync + Send> {
    async fn post_create(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_start(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn pre_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
    async fn post_stop(&self, _sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
pub trait VM: Serialize + Sync + Send {
    async fn start(&mut self) -> Result<u32>;
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()>;
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)>;
    async fn hot_detach(&mut self, id: &str) -> Result<()>;
    async fn ping(&self) -> Result<()>;
    fn socket_address(&self) -> String;
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>>;
    async fn vcpus(&self) -> Result<VcpuThreads>;
    fn pids(&self) -> Pids;
    fn container_storage_backend(&self) -> &str {
        VIRTIOFS
    }
    fn allow_bind_snapshot(&self) -> bool {
        false
    }
    fn block_image_size_overhead_percent(&self) -> u32 {
        DEFAULT_BLOCK_IMAGE_SIZE_OVERHEAD_PERCENT
    }
    fn small_dir_max_files(&self) -> usize {
        DEFAULT_SMALL_DIR_MAX_FILES
    }
    fn small_dir_max_bytes(&self) -> u64 {
        DEFAULT_SMALL_DIR_MAX_BYTES
    }
    fn overlay_image_fallback_size_mb(&self) -> u64 {
        DEFAULT_OVERLAY_IMAGE_FALLBACK_SIZE_MB
    }
    fn bind_image_fallback_size_mb(&self) -> u64 {
        DEFAULT_BIND_IMAGE_FALLBACK_SIZE_MB
    }

    /// Hotplug tap network devices that were registered via `attach()` into a running
    /// (restored) VM. Called after `vm.restore()` + `vm.resume()` for `SnapshotType::Environment`
    /// restores so the guest can see its network interfaces before `setup_sandbox()` runs.
    ///
    /// Default: no-op (implementations that do not support post-restore network hotplug
    /// return `Ok(())` silently).
    async fn hotplug_pending_network(&mut self) -> Result<()> {
        Ok(())
    }

    async fn wait_agent_ready(&self, timeout_secs: u64) -> Result<SandboxServiceClient> {
        let agent_socket = self.socket_address();
        let check_loop = async move {
            let mut delay_ms = 10u64;
            loop {
                if let Ok(client) = new_sandbox_client_fail_fast(&agent_socket).await {
                    let req = CheckRequest::new();
                    let t = Duration::from_secs(3).as_nanos() as i64;
                    if client.check(with_timeout(t), &req).await.is_ok() {
                        return client;
                    }
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms = (delay_ms * 2).min(160);
            }
        };
        tokio::time::timeout(Duration::from_secs(timeout_secs), check_loop)
            .await
            .map_err(|_| {
                anyhow!(
                    "{}s timeout waiting for agent ready after restore",
                    timeout_secs
                )
                .into()
            })
    }
}

#[macro_export]
macro_rules! impl_recoverable {
    ($ty:ty) => {
        #[async_trait]
        impl $crate::vm::Recoverable for $ty {
            async fn recover(&mut self) -> Result<()> {
                self.client = Some(self.create_client().await?);
                let pid = self.pid()?;
                let (tx, rx) = channel((0u32, 0i128));
                tokio::spawn(async move {
                    let wait_result = wait_pid(pid as i32).await;
                    tx.send(wait_result).unwrap_or_default();
                });
                self.wait_chan = Some(rx);
                Ok(())
            }
        }
    };
}

#[async_trait]
pub trait Recoverable {
    async fn recover(&mut self) -> Result<()>;
}

pub const TASK_VSOCK_FILENAME: &str = "task.vsock";
pub const CONSOLE_LOG_FILENAME: &str = "task.log";

// Pod annotations used to pass WarmFork task parameters.
// Canonical definitions live in `crate::warmfork::protocol`; these re-exports keep
// existing call sites in this crate unchanged.
pub use crate::warmfork::protocol::{
    DEFAULT_READINESS_SOCKET as ANNOTATION_WARM_FORK_DEFAULT_READINESS_SOCKET,
    PROTOCOL_V1 as WARM_FORK_PROTOCOL_V1,
    READINESS_SOCKET_ANNOTATION as ANNOTATION_WARM_FORK_READINESS_SOCKET,
    READY_PROTOCOL_ANNOTATION as ANNOTATION_WARM_FORK_READY_PROTOCOL,
    TASK_CONTEXT_ANNOTATION as ANNOTATION_WARM_FORK_TASK_CONTEXT,
    TASK_ENV_PREFIX as ANNOTATION_WARM_FORK_ENV_PREFIX,
    TASK_ID_ANNOTATION as ANNOTATION_WARM_FORK_TASK_ID,
    WARM_FORK_CONTAINERS_ANNOTATION as ANNOTATION_WARM_FORK_CONTAINERS,
};

/// Per-container injection target: the container to connect to and the task params to deliver.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WarmForkTarget {
    /// Human-readable container name (pod spec container name).
    /// Set from `kuasar.io/warm-fork-containers` at snapshot time; used for matching at restore.
    #[serde(default)]
    pub container_name: String,
    /// CRI container ID resolved at snapshot time.
    /// The guest agent uses this to look up the init PID and enter the mount namespace via setns.
    /// Empty only before restore-time overlay or for legacy templates that did not record targets.
    pub container_id: String,
    /// Inject socket path inside the container's mount namespace.
    /// Defaults to `/run/warmfork-readiness.sock` when empty.
    #[serde(default)]
    pub socket_path: String,
    /// Per-target effective env overrides (pod-level merged with per-container).
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    /// Per-target effective context.
    #[serde(default)]
    pub context: String,
}

/// Task parameters injected into a WarmFork guest after restore.
/// Covers all workload targets declared by `kuasar.io/warm-fork-containers`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WarmForkParams {
    /// Pod-level task identifier.
    ///
    /// - `None` (absent `kuasar.io/task-id` annotation) → **autonomous mode**: the guest skips
    ///   the PREPARE/READY exchange and self-starts its workload after receiving COMMIT.
    /// - `Some(id)` → **injection mode**: full PREPARE → READY → COMMIT → STARTED protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// One entry per workload container that participates in injection.
    /// In single-container mode this has exactly one element.
    pub targets: Vec<WarmForkTarget>,
    /// Phase-1 timeout in milliseconds (waiting for all READY replies).
    /// Unused in autonomous mode (no PREPARE phase).
    #[serde(default = "default_prepare_timeout_ms")]
    pub prepare_timeout_ms: u32,
    /// Phase-2 timeout in milliseconds (waiting for all STARTED replies after COMMIT).
    #[serde(default = "default_commit_timeout_ms")]
    pub commit_timeout_ms: u32,
}

fn default_prepare_timeout_ms() -> u32 {
    10_000
}

fn default_commit_timeout_ms() -> u32 {
    5_000
}

/// Sandbox-specific paths that must be updated when restoring a snapshot to a new sandbox.
/// Only per-sandbox sockets are patched; pmem/rootfs is shared read-only across sandboxes.
pub struct SnapshotPathOverrides {
    pub task_vsock: String,
    pub console_path: String,
}

impl SnapshotPathOverrides {
    /// For snapshot restore: relocate the template's socket/console files into the new
    /// sandbox directory. The vsock filename is preserved from the original; the console
    /// log path is derived from the new sandbox id so it does not inherit the template's id.
    pub fn from_original(
        new_base_dir: impl AsRef<Path>,
        original_vsock: &str,
        sandbox_id: &str,
    ) -> Self {
        let base = new_base_dir.as_ref();
        let vsock_name = Path::new(original_vsock)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(TASK_VSOCK_FILENAME);
        // TODO add log path parameter
        Self {
            task_vsock: base.join(vsock_name).to_string_lossy().into_owned(),
            console_path: format!("/tmp/{}-{}", sandbox_id, CONSOLE_LOG_FILENAME),
        }
    }
}

/// One ext4 block device to capture during a full-checkpoint snapshot.
/// Passed by the caller (which has access to sandbox storages) to `Snapshottable::snapshot`.
pub struct DiskSnapshot {
    /// Storage ID, used to derive the `.img` filename on the host.
    pub storage_id: String,
    /// CH device ID matching the `id` field in `config.json`'s `disks` array.
    pub device_id: String,
    /// Absolute path to the `.img` backing file on the host.
    pub img_path: String,
}

/// Describes a disk image stored inside a snapshot directory, used both in
/// `SnapshotMeta` (what was captured) and `RestoreSource` (what to restore).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiskImageEntry {
    pub storage_id: String,
    pub device_id: String,
    /// Path relative to the snapshot directory, e.g. `"disks/storage3.img"`.
    pub filename: String,
}

pub struct RestoreSource {
    pub snapshot_dir: PathBuf,
    pub work_dir: PathBuf,
    pub overrides: SnapshotPathOverrides,
    /// Snapshot type: drives post-restore path (Environment → setup_sandbox,
    /// WarmFork → inject_task, Continuation → transfer_network_identity).
    pub snapshot_type: SnapshotType,
    /// Disk images to restore into the new sandbox directory.
    /// Empty = template mode (disks stripped from config.json).
    /// Non-empty = full-checkpoint mode (disk files copied and paths remapped).
    pub disk_images: Vec<DiskImageEntry>,
    /// Storage entries from the snapshotted sandbox, written back into sandbox.storages so
    /// new containers with matching lower_dirs can reuse already-attached block devices.
    pub storages: Vec<vmm_common::storage::Storage>,
    /// Container IDs that were running at snapshot time (orphans on restore).
    pub orphan_container_ids: Vec<String>,
    /// Memory restore mode sent to Cloud Hypervisor in the vm.restore payload.
    pub memory_restore_mode: MemoryRestoreMode,
    /// Whether reflink CoW is supported for disk image copy.
    /// True: use reflink copy (space-efficient COW).
    /// False: use plain copy (--reflink=auto fallback).
    pub reflink_supported: bool,
    /// Share mode of the template this restore originates from.
    pub lease_mode: crate::sandbox::TemplateLeaseMode,
    /// Task parameters for WarmFork restores.  `None` for Environment/Continuation.
    pub warm_fork_params: Option<WarmForkParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    #[serde(default)]
    pub snapshot_dir: PathBuf,
    #[serde(default)]
    pub original_task_vsock: String,
    #[serde(default)]
    pub original_console_path: String,
    /// Disk images captured in this snapshot.
    /// Empty for bare-VM template snapshots; non-empty for full-checkpoint snapshots.
    #[serde(default)]
    pub disk_images: Vec<DiskImageEntry>,
    /// Share mode used when creating this snapshot.
    #[serde(default)]
    pub lease_mode: crate::sandbox::TemplateLeaseMode,
    /// Creation time for water level GC.
    #[serde(default = "default_created_at")]
    pub created_at: std::time::SystemTime,
}

fn default_created_at() -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH
}

#[async_trait]
pub trait Snapshottable {
    /// Capture VM state to `dest_dir`.  `disks` lists host-side ext4 images to copy
    /// while the VM is paused; pass an empty slice for bare-VM (template) snapshots.
    async fn snapshot(&mut self, dest_dir: &Path, disks: &[DiskSnapshot]) -> Result<SnapshotMeta> {
        let _ = (dest_dir, disks);
        Err(anyhow!("snapshot not supported for this VM type").into())
    }
    async fn restore(&mut self, src: &RestoreSource) -> Result<()> {
        let _ = src;
        Err(anyhow!("restore not supported for this VM type").into())
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct HypervisorCommonConfig {
    #[serde(default)]
    pub debug: bool,
    pub vcpus: u32,
    pub memory_in_mb: u32,
    #[serde(default)]
    pub kernel_path: String,
    #[serde(default)]
    pub image_path: String,
    #[serde(default)]
    pub initrd_path: String,
    #[serde(default)]
    pub kernel_params: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub enable_mem_prealloc: bool,
}

impl Default for HypervisorCommonConfig {
    fn default() -> Self {
        Self {
            debug: false,
            vcpus: 1,
            memory_in_mb: 1024,
            kernel_path: "/var/lib/kuasar/vmlinux.bin".to_string(),
            image_path: "".to_string(),
            initrd_path: "".to_string(),
            kernel_params: "".to_string(),
            firmware: "".to_string(),
            enable_mem_prealloc: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub enum BlockDriver {
    VirtioBlk,
    VirtioScsi,
    VirtioMmio,
}

impl Default for BlockDriver {
    fn default() -> Self {
        Self::VirtioBlk
    }
}

impl BlockDriver {
    pub fn from(s: &str) -> Self {
        match s {
            "virtio-blk" => Self::VirtioBlk,
            "virtio-scsi" => Self::VirtioScsi,
            "virtio-mmio" => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }

    pub fn to_driver_string(&self) -> String {
        match self {
            BlockDriver::VirtioBlk => "blk".to_string(),
            BlockDriver::VirtioMmio => "mmioblk".to_string(),
            BlockDriver::VirtioScsi => "scsi".to_string(),
        }
    }

    pub fn to_bus_type(&self) -> BusType {
        match self {
            BlockDriver::VirtioBlk => BusType::PCI,
            BlockDriver::VirtioMmio => BusType::NULL,
            BlockDriver::VirtioScsi => BusType::SCSI,
        }
    }

    pub fn from_bus_type(bus_type: &BusType) -> Self {
        match bus_type {
            BusType::PCI => Self::VirtioBlk,
            BusType::SCSI => Self::VirtioScsi,
            BusType::MMIO => Self::VirtioMmio,
            _ => Self::VirtioBlk,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub enum ShareFsType {
    Virtio9P,
    VirtioFS,
}

impl FromStr for ShareFsType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            VIRTIO_FS => Ok(ShareFsType::VirtioFS),
            VIRTIO_9P => Ok(ShareFsType::Virtio9P),
            _ => Err(Error::InvalidArgument(s.to_string())),
        }
    }
}

#[derive(Debug)]
pub struct VcpuThreads {
    pub vcpus: HashMap<i64, i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Pids {
    pub vmm_pid: Option<u32>,
    pub affiliated_pids: Vec<u32>,
}
