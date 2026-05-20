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
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::io::RawFd,
    },
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::{Error, Result};
use log::{debug, error, info, warn};
use nix::{errno::Errno::ESRCH, sys::signal, unistd::Pid};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{
    fs::create_dir_all,
    process::Child,
    sync::watch::{channel, Receiver, Sender},
    task::JoinHandle,
};
use tracing::instrument;
use ttrpc::context::with_timeout;
use vmm_common::{
    api::sandbox::{CheckRequest, ExecVMProcessRequest},
    SHARED_DIR_SUFFIX,
};

use crate::{
    client::{new_sandbox_client, new_sandbox_client_fail_fast},
    cloud_hypervisor::{
        block_backend::{prepare_restore_block_artifacts, rollback_pending_moves},
        client::ChClient,
        config::{
            CloudHypervisorConfig, CloudHypervisorVMConfig, ContainerStorageBackend,
            VirtioBlkConfig, VirtiofsdConfig,
        },
        devices::{
            block::Disk, vfio::VfioDevice, virtio_net::VirtioNetDevice, CloudHypervisorDevice,
        },
        snapshot::{patch_snapshot_config, validate_snapshot_config},
    },
    device::{BusType, DeviceInfo},
    param::ToCmdLineParams,
    utils::{read_std, set_cmd_fd, set_cmd_netns, wait_channel, wait_pid, write_file_atomic},
    vm::{
        DiskImageEntry, DiskSnapshot, Pids, RestoreSource, SnapshotMeta, Snapshottable,
        VcpuThreads, CONSOLE_LOG_FILENAME, TASK_VSOCK_FILENAME, VM,
    },
};

mod block_backend;
mod client;
pub mod config;
pub mod devices;
pub mod factory;
pub mod hooks;
mod memory_backend;
pub mod snapshot;

const VCPU_PREFIX: &str = "vcpu";

/// Saved metadata for a tap device that was attached to the VM before launch.
/// Used by `hotplug_pending_network()` to hotplug the tap into a restored VM.
/// The actual FDs are kept alive in `CloudHypervisorVM.fds`; this struct only
/// holds the raw fd numbers (which remain valid as long as `fds` is non-empty).
struct NetHotplugInfo {
    id: String,
    mac: String,
    /// Raw fd numbers of the tap queue FDs (valid while `CloudHypervisorVM.fds` holds them).
    raw_fds: Vec<RawFd>,
    /// Total queue count: `raw_fds.len() * 2` (one rx + one tx per fd).
    num_queues: u32,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CloudHypervisorVM {
    id: String,
    config: CloudHypervisorConfig,
    #[serde(skip)]
    devices: Vec<Box<dyn CloudHypervisorDevice + Sync + Send>>,
    netns: String,
    base_dir: String,
    agent_socket: String,
    virtiofsd_config: VirtiofsdConfig,
    #[serde(default)]
    container_storage_backend: ContainerStorageBackend,
    #[serde(default)]
    virtio_blk_config: VirtioBlkConfig,
    #[serde(skip)]
    wait_chan: Option<Receiver<(u32, i128)>>,
    #[serde(skip)]
    client: Option<ChClient>,
    #[serde(skip)]
    fds: Vec<OwnedFd>,
    pids: Pids,
    /// Tap device metadata recorded at `attach()` time for use in `hotplug_pending_network()`.
    /// Only meaningful during restore; empty for cold-booted VMs.
    #[serde(skip, default)]
    pending_net_hotplug: Vec<NetHotplugInfo>,
}

impl CloudHypervisorVM {
    pub fn new(id: &str, netns: &str, base_dir: &str, vm_config: &CloudHypervisorVMConfig) -> Self {
        let mut config = CloudHypervisorConfig::from(vm_config);
        config.api_socket = format!("{}/api.sock", base_dir);
        if !vm_config.common.initrd_path.is_empty() {
            config.initramfs = Some(vm_config.common.initrd_path.clone());
        }

        let mut virtiofsd_config = vm_config.virtiofsd.clone();
        if vm_config.container_storage_backend == ContainerStorageBackend::Virtiofs {
            virtiofsd_config.socket_path = format!("{}/virtiofs.sock", base_dir);
            virtiofsd_config.shared_dir = format!("{}/{}", base_dir, SHARED_DIR_SUFFIX);
        }
        Self {
            id: id.to_string(),
            config,
            devices: vec![],
            netns: netns.to_string(),
            base_dir: base_dir.to_string(),
            agent_socket: "".to_string(),
            virtiofsd_config,
            container_storage_backend: vm_config.container_storage_backend.clone(),
            virtio_blk_config: vm_config.virtio_blk.clone(),
            wait_chan: None,
            client: None,
            fds: vec![],
            pids: Pids::default(),
            pending_net_hotplug: vec![],
        }
    }

    pub fn add_device(&mut self, device: impl CloudHypervisorDevice + 'static) {
        self.devices.push(Box::new(device));
    }

    fn pid(&self) -> Result<u32> {
        match self.pids.vmm_pid {
            None => Err(anyhow!("empty pid from vmm_pid").into()),
            Some(pid) => Ok(pid),
        }
    }

    async fn create_client(&self) -> Result<ChClient> {
        ChClient::new(self.config.api_socket.to_string()).await
    }

    fn get_client(&mut self) -> Result<&mut ChClient> {
        self.client.as_mut().ok_or(Error::NotFound(
            "cloud hypervisor client not inited".to_string(),
        ))
    }

    async fn start_virtiofsd(&self) -> Result<u32> {
        create_dir_all(&self.virtiofsd_config.shared_dir).await?;
        let params = self.virtiofsd_config.to_cmdline_params("--");
        let mut cmd = tokio::process::Command::new(&self.virtiofsd_config.path);
        cmd.args(params.as_slice());
        debug!("start virtiofsd with cmdline: {:?}", cmd);
        set_cmd_netns(&mut cmd, self.netns.to_string())?;
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        let pid = child
            .id()
            .ok_or(anyhow!("the virtiofsd has been polled to completion"))?;
        info!("virtiofsd for {} is running with pid {}", self.id, pid);
        spawn_wait(child, format!("virtiofsd {}", self.id), None, None);
        Ok(pid)
    }

    fn append_fd(&mut self, fd: OwnedFd) -> usize {
        self.fds.push(fd);
        self.fds.len() - 1 + 3
    }

    async fn wait_stop(&mut self, t: Duration) -> Result<()> {
        if let Some(rx) = self.wait_channel().await {
            let (_, ts) = *rx.borrow();
            if ts == 0 {
                wait_channel(t, rx).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl VM for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<u32> {
        create_dir_all(&self.base_dir).await?;
        let _ = tokio::fs::remove_file(&self.config.api_socket).await;
        if self.container_storage_backend == ContainerStorageBackend::Virtiofs {
            let virtiofsd_pid = self.start_virtiofsd().await?;
            self.pids.affiliated_pids.push(virtiofsd_pid);
        }
        let mut params = self.config.to_cmdline_params("--");
        for d in self.devices.iter() {
            params.extend(d.to_cmdline_params("--"));
        }

        // the log level is single hyphen parameter, has to handle separately
        if self.config.debug {
            params.push("-vv".to_string());
        }

        // Drop cmd immediately to let the fds in pre_exec be closed.
        let child = {
            let mut cmd = tokio::process::Command::new(&self.config.path);
            cmd.args(params.as_slice());

            set_cmd_fd(&mut cmd, self.fds.drain(..).collect())?;
            set_cmd_netns(&mut cmd, self.netns.to_string())?;
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            info!("start cloud hypervisor with cmdline: {:?}", cmd);
            cmd.spawn()
                .map_err(|e| anyhow!("failed to spawn cloud hypervisor command: {}", e))?
        };
        let pid = child.id();
        info!(
            "cloud hypervisor for {} is running with pid {}",
            self.id,
            pid.unwrap_or_default()
        );
        self.pids.vmm_pid = pid;
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = channel((0u32, 0i128));
        self.wait_chan = Some(rx);
        spawn_wait(
            child,
            format!("cloud-hypervisor {}", self.id),
            Some(pid_file),
            Some(tx),
        );

        match self.create_client().await {
            Ok(client) => self.client = Some(client),
            Err(e) => {
                if let Err(re) = self.stop(true).await {
                    warn!("roll back in create clh api client: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
        };
        Ok(pid.unwrap_or_default())
    }

    #[instrument(skip_all)]
    async fn stop(&mut self, force: bool) -> Result<()> {
        let signal = if force {
            signal::SIGKILL
        } else {
            signal::SIGTERM
        };

        let pids = self.pids();
        if let Some(vmm_pid) = pids.vmm_pid {
            if vmm_pid > 0 {
                // PID reuse risk: if the VMM process exits and its PID is immediately
                // reused by an unrelated process, we could signal the wrong process.
                // The window is narrow (VMM processes are short-lived and the sandboxer
                // receives exit notification quickly). The proper fix would be pidfd_open(2)
                // (Linux 5.3+); deferred until minimum kernel version is raised.
                match signal::kill(Pid::from_raw(vmm_pid as i32), signal) {
                    Err(e) => {
                        if e != ESRCH {
                            return Err(anyhow!("kill vmm process {}: {}", vmm_pid, e).into());
                        }
                    }
                    Ok(_) => self.wait_stop(Duration::from_secs(10)).await?,
                }
            }
        }
        for affiliated_pid in pids.affiliated_pids {
            if affiliated_pid > 0 {
                // affiliated process may exits automatically, so it's ok not handle error
                signal::kill(Pid::from_raw(affiliated_pid as i32), signal).unwrap_or_default();
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn attach(&mut self, device_info: DeviceInfo) -> Result<()> {
        match device_info {
            DeviceInfo::Block(blk_info) => {
                let device = Disk::new(&blk_info.id, &blk_info.path, blk_info.read_only, true);
                self.add_device(device);
            }
            DeviceInfo::Tap(tap_info) => {
                let mut fd_ints = vec![];
                let mut raw_fds = vec![];
                for fd in tap_info.fds {
                    raw_fds.push(fd.as_raw_fd());
                    let index = self.append_fd(fd);
                    fd_ints.push(index as i32);
                }
                // Each tap fd provides one rx + one tx queue. Zero fds is invalid: CH
                // requires at least one queue pair per net device.
                assert!(
                    !raw_fds.is_empty(),
                    "tap device must have at least one queue fd"
                );
                let num_queues = raw_fds.len() as u32 * 2;
                let device = VirtioNetDevice::new(
                    &tap_info.id,
                    Some(tap_info.name),
                    &tap_info.mac_address,
                    fd_ints,
                );
                self.add_device(device);
                // Record for hotplug_pending_network() — used when restoring from snapshot.
                self.pending_net_hotplug.push(NetHotplugInfo {
                    id: tap_info.id,
                    mac: tap_info.mac_address,
                    raw_fds,
                    num_queues,
                });
            }
            DeviceInfo::Physical(vfio_info) => {
                let device = VfioDevice::new(&vfio_info.id, &vfio_info.bdf);
                self.add_device(device);
            }
            DeviceInfo::VhostUser(_vhost_user_info) => {
                return Err(anyhow!(
                    "VhostUser device attach is not supported for CloudHypervisor"
                )
                .into());
            }
            DeviceInfo::Char(_char_info) => {
                return Err(
                    anyhow!("Char device attach is not supported for CloudHypervisor").into(),
                );
            }
        };
        Ok(())
    }

    #[instrument(skip_all)]
    async fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<(BusType, String)> {
        let client = self.get_client()?;
        let addr = client.hot_attach(device_info)?;
        Ok((BusType::PCI, addr))
    }

    #[instrument(skip_all)]
    async fn hot_detach(&mut self, id: &str) -> Result<()> {
        let client = self.get_client()?;
        client.hot_detach(id)?;
        Ok(())
    }

    async fn hotplug_pending_network(&mut self) -> Result<()> {
        self.hotplug_pending_network().await
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        if self.agent_socket.is_empty() {
            return Ok(());
        }
        let client = new_sandbox_client_fail_fast(&self.agent_socket)
            .await
            .map_err(|e| anyhow!("ping: connect to agent socket: {}", e))?;
        let req = CheckRequest::new();
        client
            .check(with_timeout(Duration::from_secs(3).as_nanos() as i64), &req)
            .await
            .map_err(|e| anyhow!("ping: agent check RPC: {}", e))?;
        Ok(())
    }

    #[instrument(skip_all)]
    fn socket_address(&self) -> String {
        self.agent_socket.to_string()
    }

    #[instrument(skip_all)]
    async fn wait_channel(&self) -> Option<Receiver<(u32, i128)>> {
        self.wait_chan.clone()
    }

    #[instrument(skip_all)]
    async fn vcpus(&self) -> Result<VcpuThreads> {
        // Refer to https://github.com/firecracker-microvm/firecracker/issues/718
        Ok(VcpuThreads {
            vcpus: procfs::process::Process::new(self.pid()? as i32)
                .map_err(|e| anyhow!("failed to get process {}", e))?
                .tasks()
                .map_err(|e| anyhow!("failed to get tasks {}", e))?
                .flatten()
                .filter_map(|t| {
                    t.stat()
                        .map_err(|e| anyhow!("failed to get stat {}", e))
                        .ok()?
                        .comm
                        .strip_prefix(VCPU_PREFIX)
                        .and_then(|comm| comm.parse().ok())
                        .map(|index| (index, t.tid as i64))
                })
                .collect(),
        })
    }

    #[instrument(skip_all)]
    fn pids(&self) -> Pids {
        self.pids.clone()
    }

    fn container_storage_backend(&self) -> &str {
        self.container_storage_backend.as_str()
    }

    fn allow_bind_snapshot(&self) -> bool {
        self.virtio_blk_config.allow_bind_snapshot
    }

    fn block_image_size_overhead_percent(&self) -> u32 {
        self.virtio_blk_config.block_image_size_overhead_percent
    }

    fn small_dir_max_files(&self) -> usize {
        self.virtio_blk_config.small_dir_max_files
    }

    fn small_dir_max_bytes(&self) -> u64 {
        self.virtio_blk_config.small_dir_max_bytes
    }

    fn overlay_image_fallback_size_mb(&self) -> u64 {
        self.virtio_blk_config.overlay_image_fallback_size_mb
    }

    fn bind_image_fallback_size_mb(&self) -> u64 {
        self.virtio_blk_config.bind_image_fallback_size_mb
    }
}

#[async_trait]
impl crate::vm::Recoverable for CloudHypervisorVM {
    #[instrument(skip_all)]
    async fn recover(&mut self) -> Result<()> {
        let pid = self.pid()?;
        // Fast-fail: if the process is gone, skip the socket connect timeout.
        signal::kill(Pid::from_raw(pid as i32), None)
            .map_err(|_| anyhow!("vm process {} is no longer running", pid))?;
        if !std::path::Path::new(&self.config.api_socket).exists() {
            return Err(anyhow!(
                "api socket {} does not exist, vm process may have died",
                self.config.api_socket
            )
            .into());
        }
        self.client = Some(self.create_client().await?);
        let (tx, rx) = channel((0u32, 0i128));
        tokio::spawn(async move {
            let wait_result = wait_pid(pid as i32).await;
            tx.send(wait_result).unwrap_or_default();
        });
        self.wait_chan = Some(rx);
        Ok(())
    }
}

impl CloudHypervisorVM {
    fn vsock_path(&self) -> String {
        format!("{}/{}", self.base_dir, TASK_VSOCK_FILENAME)
    }

    fn console_log_path(&self) -> String {
        format!("{}/{}", self.base_dir, CONSOLE_LOG_FILENAME)
    }

    async fn sync_guest_fs(&self) -> Result<()> {
        let client = new_sandbox_client(&self.agent_socket).await?;
        let timeout_ns = Duration::from_secs(10).as_nanos() as i64;
        let mut req = ExecVMProcessRequest::new();
        req.command = "sync".to_string();
        client
            .exec_vm_process(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("guest sync: {}", e))?;
        Ok(())
    }

    async fn launch_for_restore(&mut self) -> Result<()> {
        create_dir_all(&self.base_dir).await?;
        // Remove any stale socket left by a previous failed restore attempt.
        let _ = tokio::fs::remove_file(&self.config.api_socket).await;
        let child = {
            let mut cmd = tokio::process::Command::new(&self.config.path);
            cmd.arg("--api-socket").arg(&self.config.api_socket);
            if self.config.debug {
                cmd.arg("-vv");
            }
            set_cmd_netns(&mut cmd, self.netns.to_string())?;
            // self.fds is intentionally not passed here: fd-bearing devices (vhost-user, tap)
            // are not supported in restore mode.
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            info!("start cloud-hypervisor for restore: {:?}", cmd);
            cmd.spawn()
                .map_err(|e| anyhow!("failed to spawn cloud-hypervisor for restore: {}", e))?
        };
        let pid = child.id();
        self.pids.vmm_pid = pid;
        let pid_file = format!("{}/pid", self.base_dir);
        let (tx, rx) = channel((0u32, 0i128));
        self.wait_chan = Some(rx);
        spawn_wait(
            child,
            format!("cloud-hypervisor-restore {}", self.id),
            Some(pid_file),
            Some(tx),
        );
        match self.create_client().await {
            Ok(client) => self.client = Some(client),
            Err(e) => {
                if let Err(re) = self.stop(true).await {
                    warn!("rollback in restore launch: {}", re);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Hotplug tap devices recorded at `attach()` time into the running (restored) VM.
    ///
    /// Called after `vm.restore()` + `vm.resume()` for `SnapshotType::Environment` restores,
    /// so the guest can see the network interfaces before `setup_sandbox()` configures them.
    /// Each tap FD in `self.fds` is sent to CH via `vm.add-net` (SCM_RIGHTS).
    pub(crate) async fn hotplug_pending_network(&mut self) -> Result<()> {
        if self.pending_net_hotplug.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_net_hotplug);
        // If any tap hotplug fails the caller (try_restore) calls rollback_vm() which
        // stops the entire VM process. Individual tap detach is therefore not required
        // on partial failure — the VM is torn down as a unit.
        for info in &pending {
            let client = self.get_client()?;
            client.vm_add_net(&info.id, &info.mac, info.num_queues, info.raw_fds.clone())?;
            info!(
                "VM {}: hotplugged tap {} (mac={}, num_queues={})",
                self.id, info.id, info.mac, info.num_queues
            );
        }
        Ok(())
    }
}

#[async_trait]
impl Snapshottable for CloudHypervisorVM {
    async fn snapshot(
        &mut self,
        dest_dir: &std::path::Path,
        disks: &[DiskSnapshot],
    ) -> Result<SnapshotMeta> {
        let t0 = Instant::now();
        let id = self.id.clone();
        tokio::fs::create_dir_all(dest_dir).await?;

        // Flush guest ext4 journals so snapshot captures fully-committed state.
        if let Err(e) = self.sync_guest_fs().await {
            warn!(
                "guest sync before snapshot failed, snapshot may be inconsistent: {}",
                e
            );
        }
        info!(
            "snapshot {id}: guest sync done in {}ms",
            t0.elapsed().as_millis()
        );

        let client = self.get_client()?;

        // Freeze CPU and all device queues for a consistent point-in-time capture.
        client.vm_pause().map_err(|e| anyhow!("vm.pause: {}", e))?;
        info!("snapshot {id}: vm paused in {}ms", t0.elapsed().as_millis());

        // Copy disk images while the VM is paused: guest journals are already flushed,
        // device I/O queues are frozen, so the .img file is at a consistent checkpoint.
        let disk_images = if !disks.is_empty() {
            let disks_dir = dest_dir.join("disks");
            match copy_disk_images(disks, dest_dir, &disks_dir).await {
                Ok(entries) => {
                    info!(
                        "snapshot {id}: {} disk image(s) copied in {}ms",
                        entries.len(),
                        t0.elapsed().as_millis()
                    );
                    entries
                }
                Err(e) => {
                    // Resume before propagating the error so the VM doesn't stay paused.
                    if let Err(re) = client.vm_resume() {
                        error!("vm.resume after disk-copy failure: {}", re);
                    }
                    return Err(anyhow!("snapshot {id}: disk copy failed: {}", e).into());
                }
            }
        } else {
            vec![]
        };

        let dest_url = format!("file://{}", dest_dir.display());
        let snap_result = client.vm_snapshot(&dest_url);

        // Always resume — a stuck VM is worse than a skipped snapshot.
        if let Err(e) = client.vm_resume() {
            error!("vm.resume after snapshot failed: {}", e);
        }
        info!(
            "snapshot {id}: vm.snapshot + resume done in {}ms",
            t0.elapsed().as_millis()
        );

        snap_result.map_err(|e| anyhow!("vm.snapshot: {}", e))?;

        Ok(SnapshotMeta {
            snapshot_dir: dest_dir.to_path_buf(),
            original_task_vsock: self.vsock_path(),
            original_console_path: self.console_log_path(),
            disk_images,
            lease_mode: crate::sandbox::TemplateLeaseMode::default(),
            created_at: std::time::SystemTime::now(),
        })
    }

    async fn restore(&mut self, src: &RestoreSource) -> Result<()> {
        let t0 = Instant::now();
        tokio::fs::create_dir_all(&src.work_dir).await?;

        // 1. For full-checkpoint snapshots: prepare restored block images.
        //    For MovedExclusive (Continuation), disk files are renamed into base_dir here.
        //    If any subsequent step fails we must rename them back so the template remains
        //    intact and can be retried or returned to the pool.
        let restored_blocks =
            prepare_restore_block_artifacts(src, std::path::Path::new(&self.base_dir)).await?;
        if !restored_blocks.restored_paths.is_empty() {
            info!(
                "restore {}: {} disk image(s) {} in {}ms",
                self.id,
                restored_blocks.restored_paths.len(),
                restored_blocks.action.as_log_str(),
                t0.elapsed().as_millis(),
            );
        }

        // Macro used for steps 2–4: roll back moved disks before propagating errors.
        // CH has not yet started at this point, so no process kill is needed here.
        macro_rules! restore_try {
            ($result:expr) => {
                match $result {
                    Ok(v) => v,
                    Err(e) => {
                        if let Err(re) = rollback_pending_moves(&restored_blocks).await {
                            warn!(
                                "restore {}: disk move rollback failed ({}); \
                                 template snapshot may be missing disk files",
                                self.id, re
                            );
                        }
                        return Err(e.into());
                    }
                }
            };
        }

        // 2. Validate and write patched config.json (sandbox-specific socket paths updated,
        //    disk paths remapped or stripped depending on mode).
        restore_try!(
            validate_snapshot_config(&src.snapshot_dir.join("config.json"), &src.snapshot_type)
                .await
        );
        restore_try!(
            patch_snapshot_config(
                &src.snapshot_dir.join("config.json"),
                &src.work_dir.join("config.json"),
                &src.overrides,
                &restored_blocks.disk_remaps,
            )
            .await
        );

        // 3. Symlink/pin immutable snapshot memory artifacts into the per-sandbox work dir.
        let prepared_memory = restore_try!(memory_backend::prepare_memory_backend(src).await);

        // 4. Start CH with only --api-socket; all VM config comes from the snapshot.
        restore_try!(self.launch_for_restore().await);
        info!(
            "restore {}: CH process ready in {}ms",
            self.id,
            t0.elapsed().as_millis()
        );

        // Steps 5–6: CH is running; failures must kill CH then roll back disk moves.
        macro_rules! restore_try_with_kill {
            ($result:expr, $label:literal) => {
                if let Err(e) = $result {
                    if let Err(ke) = self.stop(true).await {
                        warn!("restore {}: kill CH after {}: {}", self.id, $label, ke);
                    }
                    if let Err(re) = rollback_pending_moves(&restored_blocks).await {
                        warn!(
                            "restore {}: disk move rollback failed after {} ({}); \
                             template snapshot may be missing disk files",
                            self.id, $label, re
                        );
                    }
                    return Err(e.into());
                }
            };
        }

        // 5. Trigger restore; CH loads config.json + state.json + memory-ranges from work_dir.
        // CH restores vCPUs in a paused state (snapshot was taken while paused).
        // Call vm.resume immediately after so the guest starts executing.
        let api_result: anyhow::Result<()> = async {
            let client = self.get_client()?;
            client
                .vm_restore(&prepared_memory.source_url, &prepared_memory.mode)
                .map_err(|e| anyhow!("vm.restore API: {}", e))?;
            client
                .vm_resume()
                .map_err(|e| anyhow!("vm.resume after restore: {}", e))?;
            Ok(())
        }
        .await;
        restore_try_with_kill!(api_result, "API failure");
        info!(
            "restore {}: vm.restore+resume done in {}ms",
            self.id,
            t0.elapsed().as_millis()
        );

        // 6. Wait for the guest agent to come back up over hvsock (15 s timeout).
        //
        // NOTE: the sandbox layer's `init_client()` (called in `try_restore()`) also waits
        // for agent readiness via `new_sandbox_client()`.  This pre-check is kept here so
        // that a timeout at the VM level kills the CH process with a clear error before the
        // sandbox layer attempts to connect, avoiding a longer implicit retry in init_client.
        // If init_client gains its own explicit timeout, remove this check and let init_client
        // be the single point of truth for agent readiness.
        let agent_result = self
            .wait_agent_ready(15)
            .await
            .map_err(|e| anyhow!("restore {}: agent not ready: {}", self.id, e));
        restore_try_with_kill!(agent_result, "agent timeout");
        info!(
            "restore {}: agent ready, total restore time {}ms",
            self.id,
            t0.elapsed().as_millis()
        );

        Ok(())
    }
}

// Copy disk images into the snapshot's `disks/` subdirectory while the VM is paused.
// Returns a `DiskImageEntry` per disk on success, or an error if any copy fails.
// Created as a separate function so `snapshot()` can resume the VM before returning the error.
async fn copy_disk_images(
    disks: &[DiskSnapshot],
    dest_dir: &std::path::Path,
    disks_dir: &std::path::Path,
) -> containerd_sandbox::error::Result<Vec<DiskImageEntry>> {
    tokio::fs::create_dir_all(disks_dir)
        .await
        .map_err(|e| anyhow!("create disks dir: {}", e))?;

    let mut entries = Vec::with_capacity(disks.len());
    for disk in disks {
        let filename = format!("disks/{}.img", disk.storage_id);
        let dst = dest_dir.join(&filename);
        copy_img_file(std::path::Path::new(&disk.img_path), &dst)
            .await
            .map_err(|e| anyhow!("copy {}: {}", disk.storage_id, e))?;
        entries.push(DiskImageEntry {
            storage_id: disk.storage_id.clone(),
            device_id: disk.device_id.clone(),
            filename,
        });
    }
    Ok(entries)
}

// Copy a file atomically via a temp path, attempting CoW reflink first for near-zero
// cost on btrfs/XFS, falling back to a standard byte copy on other filesystems.
async fn copy_img_file(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> containerd_sandbox::error::Result<()> {
    let tmp_name = format!(
        ".{}.tmp",
        dst.file_name().unwrap_or_default().to_string_lossy()
    );
    let tmp = dst.with_file_name(tmp_name);
    let src_s = src.to_string_lossy().to_string();
    let tmp_s = tmp.to_string_lossy().to_string();

    let reflink_ok = tokio::process::Command::new("cp")
        .args(["--reflink=auto", &src_s, &tmp_s])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    if !reflink_ok {
        // Remove any partial file left by a failed cp before falling back.
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::copy(src, &tmp)
            .await
            .map_err(|e| anyhow!("copy {:?} -> {:?}: {}", src, dst, e))?;
    }

    tokio::fs::rename(&tmp, dst)
        .await
        .map_err(|e| anyhow!("rename {:?} -> {:?}: {}", tmp, dst, e))?;
    Ok(())
}

// Strict reflink copy - fails if filesystem doesn't support it.
// Used for Shared mode restore where COW isolation is required.
// Writes to a temp path first and renames atomically on success.
async fn copy_img_file_reflink(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> containerd_sandbox::error::Result<()> {
    let tmp_name = format!(
        ".{}.tmp",
        dst.file_name().unwrap_or_default().to_string_lossy()
    );
    let tmp = dst.with_file_name(tmp_name);
    let src_s = src.to_string_lossy().to_string();
    let tmp_s = tmp.to_string_lossy().to_string();

    let status = tokio::process::Command::new("cp")
        .args(["--reflink=always", &src_s, &tmp_s])
        .status()
        .await
        .map_err(|e| anyhow!("reflink copy {:?} -> {:?}: {}", src, dst, e))?;

    if !status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow!(
            "reflink copy failed (filesystem doesn't support CoW): {:?} -> {:?}. \
            Required: XFS with reflink=1 or btrfs for snapshot shared mode. Status: {:?}",
            src,
            dst,
            status
        )
        .into());
    }

    tokio::fs::rename(&tmp, dst)
        .await
        .map_err(|e| anyhow!("rename {:?} -> {:?}: {}", tmp, dst, e))?;
    Ok(())
}

// Symlink src → dst, treating EEXIST as success (idempotent).
async fn symlink_idempotent(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    name: &str,
) -> containerd_sandbox::error::Result<()> {
    match tokio::fs::symlink(src.as_ref(), dst.as_ref()).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow!("symlink {}: {}", name, e).into()),
    }
}

// Test if reflink CoW is supported between sandboxer_working_dir and store_dir.
// Returns Ok(true) if reflink works, Ok(false) if not supported, Err on I/O failure.
//
// The test writes src into sandboxer_working_dir and dst into store_dir.
// Actual restore copies in the opposite direction (store_dir → sandbox dir), but
// reflink requires both paths on the same filesystem, so either direction gives the
// same result.
pub async fn check_reflink_support(
    sandboxer_working_dir: &Path,
    store_dir: &Path,
) -> containerd_sandbox::error::Result<bool> {
    let test_src = sandboxer_working_dir.join(".reflink_test_src");
    let test_dst = store_dir.join(".reflink_test_dst");

    tokio::fs::write(&test_src, b"test content for reflink")
        .await
        .map_err(|e| anyhow!("create reflink test file: {}", e))?;

    let status = tokio::process::Command::new("cp")
        .arg("--reflink=always")
        .arg(&test_src)
        .arg(&test_dst)
        .status()
        .await
        .map_err(|e| anyhow!("reflink test copy: {}", e))?;

    let _ = tokio::fs::remove_file(&test_src).await;
    let _ = tokio::fs::remove_file(&test_dst).await;
    Ok(status.success())
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::template::SnapshotType;

    /// Verify that wait_agent_ready times out promptly when no agent is listening.
    #[tokio::test]
    async fn test_wait_agent_ready_times_out() {
        let vm = CloudHypervisorVM {
            agent_socket: "hvsock:///nonexistent-socket-path.vsock:1024".to_string(),
            ..Default::default()
        };
        let result = vm.wait_agent_ready(1).await;
        assert!(result.is_err(), "expected timeout error");
        let Err(e) = result else {
            panic!("expected timeout error")
        };
        let msg = e.to_string();
        assert!(
            msg.contains("timeout"),
            "expected 'timeout' in error, got: {msg}"
        );
    }

    /// End-to-end snapshot → restore roundtrip.
    ///
    /// Requires cloud-hypervisor binary, kernel, and rootfs.  Set the env vars:
    ///   CH_BINARY, CH_KERNEL, CH_ROOTFS
    /// Run with:
    ///   cargo test -p vmm-sandboxer -- --ignored snapshot_restore_roundtrip
    #[tokio::test]
    #[ignore = "requires host CH binary, kernel, and rootfs; set CH_BINARY/CH_KERNEL/CH_ROOTFS"]
    async fn snapshot_restore_roundtrip() {
        use temp_dir::TempDir;

        use crate::{
            cloud_hypervisor::{
                config::CloudHypervisorVMConfig,
                devices::{console::Console, pmem::Pmem, vsock::Vsock},
            },
            vm::{RestoreSource, SnapshotPathOverrides, Snapshottable, TASK_VSOCK_FILENAME, VM},
        };

        let ch_binary = std::env::var("CH_BINARY")
            .unwrap_or_else(|_| "/usr/local/bin/cloud-hypervisor".to_string());
        let kernel =
            std::env::var("CH_KERNEL").unwrap_or_else(|_| "/var/lib/kuasar/vmlinux".to_string());
        let rootfs =
            std::env::var("CH_ROOTFS").unwrap_or_else(|_| "/var/lib/kuasar/rootfs.img".to_string());

        for p in [&ch_binary, &kernel, &rootfs] {
            if !std::path::Path::new(p).exists() {
                eprintln!("snapshot_restore_roundtrip: skipping, {p} not found");
                return;
            }
        }

        let tmp = TempDir::new().unwrap();

        // ── template VM ──────────────────────────────────────────────────────
        let tmpl_dir = tmp.path().join("sandbox-template");
        tokio::fs::create_dir_all(&tmpl_dir).await.unwrap();

        let mut vm_config = CloudHypervisorVMConfig::default();
        vm_config.path = ch_binary.clone();
        vm_config.common.kernel_path = kernel.clone();
        vm_config.common.image_path = rootfs.clone();

        let mut tmpl_vm =
            CloudHypervisorVM::new("tmpl", "", tmpl_dir.to_str().unwrap(), &vm_config);
        tmpl_vm.add_device(Pmem::new("rootfs", &rootfs, true));
        let tmpl_vsock = format!("{}/{}", tmpl_dir.display(), TASK_VSOCK_FILENAME);
        tmpl_vm.add_device(Vsock::new(3, &tmpl_vsock, "vsock"));
        tmpl_vm.agent_socket = format!("hvsock://{}:1024", tmpl_vsock);
        tmpl_vm.add_device(Console::new(
            &format!("/tmp/tmpl-{}", CONSOLE_LOG_FILENAME),
            "console",
        ));

        let t_cold = Instant::now();
        tmpl_vm
            .start()
            .await
            .expect("template VM cold start failed");
        eprintln!("cold start: {}ms", t_cold.elapsed().as_millis());

        // snapshot (bare-VM, no disk images)
        let snapshot_dir = tmp.path().join("snapshot");
        let meta = tmpl_vm
            .snapshot(&snapshot_dir, &[])
            .await
            .expect("snapshot failed");
        eprintln!("snapshot: {:?}", meta);

        tmpl_vm.stop(true).await.unwrap();

        // ── restore into a new sandbox ────────────────────────────────────────
        let restore_dir = tmp.path().join("sandbox-restore");
        tokio::fs::create_dir_all(&restore_dir).await.unwrap();

        let mut restore_vm =
            CloudHypervisorVM::new("restore", "", restore_dir.to_str().unwrap(), &vm_config);
        let overrides = SnapshotPathOverrides::from_original(
            &restore_dir,
            &meta.original_task_vsock,
            "restore",
        );
        restore_vm.agent_socket = format!("hvsock://{}:1024", overrides.task_vsock);

        let src = RestoreSource {
            snapshot_dir: snapshot_dir.clone(),
            work_dir: restore_dir.join("restore-work"),
            overrides,
            snapshot_type: SnapshotType::Environment,
            disk_images: vec![],
            storages: vec![],
            orphan_container_ids: vec![],
            memory_restore_mode: crate::sandbox::MemoryRestoreMode::default(),
            reflink_supported: false,
            lease_mode: crate::sandbox::TemplateLeaseMode::Exclusive,
            warm_fork_params: None,
        };

        let t_restore = Instant::now();
        restore_vm.restore(&src).await.expect("restore failed");
        let restore_ms = t_restore.elapsed().as_millis();
        eprintln!("restore (agent ready): {}ms", restore_ms);

        let timeout_ms: u128 = std::env::var("RESTORE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(800);
        assert!(
            restore_ms < timeout_ms,
            "restore took {restore_ms}ms, exceeds {timeout_ms}ms target \
             (override with RESTORE_TIMEOUT_MS env var)"
        );

        restore_vm.stop(true).await.unwrap();
    }
}

macro_rules! read_stdio {
    ($stdio:expr, $cmd_name:ident) => {
        if let Some(std) = $stdio {
            let cmd_name_clone = $cmd_name.clone();
            tokio::spawn(async move {
                read_std(std, &cmd_name_clone).await.unwrap_or_default();
            });
        }
    };
}

fn spawn_wait(
    child: Child,
    cmd_name: String,
    pid_file_path: Option<String>,
    exit_chan: Option<Sender<(u32, i128)>>,
) -> JoinHandle<()> {
    let mut child = child;
    tokio::spawn(async move {
        if let Some(pid_file) = pid_file_path {
            if let Some(pid) = child.id() {
                write_file_atomic(&pid_file, &pid.to_string())
                    .await
                    .unwrap_or_default();
            }
        }

        read_stdio!(child.stdout.take(), cmd_name);
        read_stdio!(child.stderr.take(), cmd_name);

        match child.wait().await {
            Ok(status) => {
                if !status.success() {
                    error!("{} exit {}", cmd_name, status);
                }
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((
                        status.code().unwrap_or_default() as u32,
                        now.unix_timestamp_nanos(),
                    ))
                    .unwrap_or_default();
                }
            }
            Err(e) => {
                error!("{} wait error {}", cmd_name, e);
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((0, now.unix_timestamp_nanos())).unwrap_or_default();
                }
            }
        }
    })
}
