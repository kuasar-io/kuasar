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
    collections::HashMap, io::ErrorKind, os::unix::fs::PermissionsExt, path::Path, time::Duration,
};

use anyhow::anyhow;
use containerd_sandbox::{
    error::{Error, Result},
    spec::Mount,
};
use containerd_shim::mount::mount_rootfs;
use log::{debug, warn};
use nix::libc::MNT_DETACH;
use ttrpc::context::with_timeout;
pub use utils::*;
use vmm_common::{
    api::sandbox::AdoptContainerRequest,
    mount::{bind_mount, unmount, MNT_NOFOLLOW},
    storage::{Storage, DRIVEREPHEMERALTYPE},
    KUASAR_STATE_DIR,
};

use crate::{
    device::{BlockDeviceInfo, DeviceInfo},
    sandbox::{KuasarSandbox, KUASAR_GUEST_SHARE_DIR},
    storage::{
        attach::{BlockAttachTransaction, BlockStorageMetadata},
        block_provider::{
            BlockArtifact, BlockFs, BlockPrepareRequest, BlockProvider, LocalBlockProvider,
        },
        guest_file::{
            join_guest_component, scan_dir, GuestFileInjector, ScannedDir, DRIVER_GUEST_FILE,
        },
        mount::{get_mount_info, is_bind, is_bind_shm, is_overlay},
    },
    vm::{BlockDriver, VIRTIO_BLK, VM},
};

pub(crate) mod attach;
pub(crate) mod block_provider;
pub(crate) mod device_graph;
pub(crate) mod guest_file;
pub mod mount;
pub mod utils;

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    pub async fn attach_storage(
        &mut self,
        container_id: &str,
        m: &Mount,
        is_rootfs_mount: bool,
    ) -> Result<()> {
        // Orphan detection: find if a reusable storage exists and whether it is
        // referenced by an orphan container from a prior WarmFork restore.  We do
        // this with an immutable scan first to avoid borrow conflicts with the
        // mutable storage ref below.
        let orphan_match: Option<(String, String)> = if !self.orphan_container_ids.is_empty() {
            self.find_orphan_for_mount(m, is_rootfs_mount)
        } else {
            None
        };

        if let Some((orphan_id, _storage_id)) = orphan_match {
            match self.call_adopt_container(&orphan_id, container_id).await {
                Ok(()) => {
                    // Adoption succeeded: transfer storage ownership and remove from orphan list.
                    if let Some(storage) = self.find_reusable_storage(m, is_rootfs_mount) {
                        storage.defer(&orphan_id);
                        storage.refer(container_id);
                    }
                    self.orphan_container_ids.retain(|id| *id != orphan_id);
                    return Ok(());
                }
                Err(e) => {
                    return Err(anyhow!(
                        "attach_storage: adopt_container '{}' -> '{}' failed ({}); \
                         refusing to co-run restored orphan with a new container",
                        orphan_id,
                        container_id,
                        e
                    )
                    .into());
                }
            }
        }

        if let Some(storage) = self.find_reusable_storage(m, is_rootfs_mount) {
            storage.refer(container_id);
            return Ok(());
        }

        let id = format!("storage{}", self.increment_and_get_id());
        debug!(
            "attach storage to container {} for mount {:?} with id {}",
            container_id, m, id
        );

        if is_block_device(&*m.source).await? {
            self.handle_block_device(&id, container_id, m).await?;
            return Ok(());
        }
        // handle tmpfs mount
        let mount_info = get_mount_info(&m.source).await?;
        if let Some(mi) = mount_info {
            // Only allow use tmpfs in emptyDir
            if mi.fs_type == "tmpfs" && mi.mount_point.contains("kubernetes.io~empty-dir") {
                self.handle_tmpfs_mount(&id, container_id, m, &mi).await?;
                return Ok(());
            }
        }
        if is_bind_shm(m) {
            return Ok(());
        }

        if is_bind(m) {
            if self.vm.container_storage_backend() == VIRTIO_BLK {
                self.handle_bind_mount_blk(&id, container_id, m, is_rootfs_mount)
                    .await?;
            } else {
                self.handle_bind_mount(&id, container_id, m).await?;
            }
            return Ok(());
        }

        if is_overlay(m) {
            if self.vm.container_storage_backend() == VIRTIO_BLK {
                self.handle_overlay_mount_blk(&id, container_id, m).await?;
            } else {
                self.handle_overlay_mount(&id, container_id, m).await?;
            }
            return Ok(());
        }

        Ok(())
    }

    /// Call the in-guest SandboxService's AdoptContainer RPC so the task service
    /// renames the orphan container (`old_id`) to `new_id`.  This must be called
    /// before containerd sends `Create(new_id)` to avoid spawning a duplicate process.
    async fn call_adopt_container(&self, old_id: &str, new_id: &str) -> Result<()> {
        let client_guard = self.client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow!("call_adopt_container: no sandbox client available"))?;
        let mut req = AdoptContainerRequest::new();
        req.old_id = old_id.to_string();
        req.new_id = new_id.to_string();
        let timeout_ns = Duration::from_secs(10).as_nanos() as i64;
        client
            .adopt_container(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("AdoptContainer RPC failed: {}", e))?;
        Ok(())
    }

    pub async fn deference_storage(&mut self, container_id: &str, m: &Mount) -> Result<()> {
        if let Some(s) = self
            .storages
            .iter_mut()
            .find(|s| s.is_for_mount(m) && s.ref_container.contains_key(container_id))
        {
            s.defer(container_id);
        }
        self.gc_storages().await?;
        Ok(())
    }

    fn find_orphan_for_mount(&self, m: &Mount, is_rootfs_mount: bool) -> Option<(String, String)> {
        device_graph::find_orphan_for_mount(
            &self.storages,
            &self.orphan_container_ids,
            m,
            is_rootfs_mount,
        )
    }

    fn find_reusable_storage(&mut self, m: &Mount, is_rootfs_mount: bool) -> Option<&mut Storage> {
        let index = self.storages.find_reusable_index(m, is_rootfs_mount)?;
        self.storages.get_mut(index)
    }

    async fn handle_block_device(&mut self, id: &str, container_id: &str, m: &Mount) -> Result<()> {
        let read_only = m.options.contains(&"ro".to_string());
        let source = if m.source.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "mount source should exist for block device {:?}",
                m
            )));
        } else {
            m.source.clone()
        };
        let device_id = format!("blk{}", self.increment_and_get_id());
        let (bus_type, addr) = self
            .vm
            .hot_attach(DeviceInfo::Block(BlockDeviceInfo {
                id: device_id.to_string(),
                path: source.clone(),
                read_only,
            }))
            .await?;
        // only pass options "ro" to agent, as other mount options may belongs to bind mount only.
        // we have to support mounting the block device(such as /dev/sda) to a directory,
        // but CRI support only bind mount, so the mount options here, which is added by containerd,
        // belongs to bind mount, this is the "storage mount" here, agent will mount block device,
        // so the mount options may not be suitable.
        let options = if read_only {
            vec!["ro".to_string()]
        } else {
            vec![]
        };

        let mut storage = Storage {
            host_source: m.source.clone(),
            r#type: m.r#type.clone(),
            id: id.to_string(),
            device_id: Some(device_id.to_string()),
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: addr.to_string(),
            driver: BlockDriver::from_bus_type(&bus_type).to_driver_string(),
            driver_options: vec![],
            fstype: get_fstype(&source).await?,
            options,
            mount_point: format!("{}{}", KUASAR_GUEST_SHARE_DIR, id),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: Some(format!("block:{}", source)),
        };

        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }

    async fn handle_bind_mount(
        &mut self,
        storage_id: &str,
        container_id: &str,
        m: &Mount,
    ) -> Result<()> {
        let source = if m.source.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "mount source should exist for bind mount {:?}",
                m
            )));
        } else {
            m.source.clone()
        };

        let options = if m.options.contains(&"ro".to_string()) {
            vec!["ro".to_string()]
        } else {
            vec![]
        };

        let host_dest = format!("{}/{}", self.get_sandbox_shared_path(), &storage_id);
        debug!("bind mount storage for mount {:?}, dest: {}", m, &host_dest);
        let source_path = Path::new(&*source);
        if source_path.is_dir() {
            tokio::fs::create_dir_all(&host_dest).await?;
        } else {
            let is_regular = is_regular_file(source_path).await?;
            if !is_regular {
                return Err(Error::InvalidArgument(format!(
                    "file {} is not a regular file, can not be the mount source",
                    source
                )));
            }
            tokio::fs::File::create(&host_dest).await?;
        }
        bind_mount(&*source, &host_dest, &m.options)?;
        let mut storage = Storage {
            host_source: source.clone(),
            r#type: m.r#type.clone(),
            id: storage_id.to_string(),
            device_id: None,
            ref_container: Default::default(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: "".to_string(),
            driver_options: vec![],
            fstype: "bind".to_string(),
            options,
            mount_point: format!("{}/{}", KUASAR_STATE_DIR, &storage_id),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: Some(format!("bind:{}", source)),
        };

        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }

    async fn handle_overlay_mount(
        &mut self,
        storage_id: &str,
        container_id: &str,
        m: &Mount,
    ) -> Result<()> {
        if m.source.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "mount source should exist for bind mount {:?}",
                m
            )));
        }

        let options = if m.options.contains(&"ro".to_string()) {
            vec!["ro".to_string()]
        } else {
            vec![]
        };

        let host_dest = format!("{}/{}", self.get_sandbox_shared_path(), &storage_id);
        debug!("overlay mount storage for {:?}, dest: {}", m, &host_dest);
        tokio::fs::create_dir_all(&host_dest).await?;
        mount_rootfs(Some(&m.r#type), Some(&m.source), &m.options, &host_dest)
            .map_err(|e| anyhow!("mount rootfs: {}", e))?;

        let mut storage = Storage {
            host_source: m.source.clone(),
            r#type: m.r#type.clone(),
            id: storage_id.to_string(),
            device_id: None,
            ref_container: Default::default(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: "".to_string(),
            driver_options: vec![],
            fstype: "bind".to_string(),
            options,
            mount_point: format!("{}/{}", KUASAR_STATE_DIR, &storage_id),
            lower_dirs: extract_lower_dirs(&m.options),
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: extract_lower_dirs(&m.options)
                .map(|lower| format!("overlay:{}", lower)),
        };

        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }

    async fn handle_tmpfs_mount(
        &mut self,
        storage_id: &str,
        container_id: &str,
        m: &Mount,
        mount_info: &MountInfo,
    ) -> Result<()> {
        let options = if m.options.contains(&"ro".to_string()) {
            vec!["ro".to_string()]
        } else {
            vec![]
        };

        let mut storage = Storage {
            host_source: m.source.clone(),
            r#type: m.r#type.clone(),
            id: storage_id.to_string(),
            device_id: None,
            ref_container: Default::default(),
            need_guest_handle: true,
            source: "tmpfs".to_string(),
            driver: DRIVEREPHEMERALTYPE.to_string(),
            driver_options: vec![],
            fstype: "tmpfs".to_string(),
            options,
            mount_point: format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: Some(format!("tmpfs:{}", m.source)),
        };
        // only handle size option because other options may not supported in guest
        for o in &mount_info.options {
            if o.starts_with("size=") {
                storage.options.push(o.to_string());
            }
        }
        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }

    async fn gc_storages(&mut self) -> Result<()> {
        let storage_infos: Vec<Storage> = self
            .storages
            .iter()
            .filter(|&x| x.ref_count() == 0)
            .cloned()
            .collect();
        for storage in storage_infos {
            self.detach_storage(&storage).await?;
            self.storages.retain(|x| x.id != storage.id);
        }
        Ok(())
    }

    async fn detach_storage(&mut self, storage: &Storage) -> Result<()> {
        if let Some(did) = &storage.device_id {
            self.vm.hot_detach(did).await?;
            if storage.owned_by_runtime {
                let artifact = BlockArtifact::from_storage(storage).ok_or_else(|| {
                    anyhow!(
                        "runtime-owned storage {} is missing cleanup_path",
                        storage.id
                    )
                })?;
                LocalBlockProvider.release(&artifact).await?;
            }
        } else if storage.driver == DRIVER_GUEST_FILE {
            // DRIVER_GUEST_FILE type: file pushed to guest, no host-side resource to clean up.
        } else if storage.fstype == "bind" {
            let mount_point = format!("{}/{}", self.get_sandbox_shared_path(), storage.id);
            unmount(&mount_point, MNT_DETACH | MNT_NOFOLLOW)?;
            if Path::new(&mount_point).is_dir() {
                if let Err(e) = tokio::fs::remove_dir(&mount_point).await {
                    if e.kind() != ErrorKind::NotFound {
                        return Err(anyhow!(
                            "failed to remove dir of storage {}, {}",
                            mount_point,
                            e
                        )
                        .into());
                    }
                }
            } else if let Err(e) = tokio::fs::remove_file(&mount_point).await {
                if e.kind() != ErrorKind::NotFound {
                    return Err(
                        anyhow!("failed to remove file of storage {}, {}", mount_point, e).into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn deference_container_storages(&mut self, container_id: &str) -> Result<()> {
        for storage in self.storages.iter_mut() {
            if storage.ref_container.contains_key(container_id) {
                storage.defer(container_id);
            }
        }
        self.gc_storages().await?;
        Ok(())
    }

    // --- virtio-blk container layer handlers ---

    async fn handle_overlay_mount_blk(
        &mut self,
        storage_id: &str,
        container_id: &str,
        m: &Mount,
    ) -> Result<()> {
        if m.source.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "mount source should exist for overlay mount {:?}",
                m
            )));
        }

        // Step 1: mount overlay on host to a temporary directory
        let overlay_dir = format!("{}/overlay-{}", self.base_dir, storage_id);
        tokio::fs::create_dir_all(&overlay_dir).await?;
        if let Err(e) = mount_rootfs(Some(&m.r#type), Some(&m.source), &m.options, &overlay_dir) {
            let _ = tokio::fs::remove_dir_all(&overlay_dir).await;
            return Err(anyhow!("mount overlay for blk: {}", e).into());
        }

        // Steps 2-4: create block image and copy overlay content into it.
        // Shared mode + reflink_supported uses XFS with reflink=1 for COW efficiency.
        // Shared mode without reflink or Exclusive mode uses ext4 (default).
        let img_path = format!("{}/{}.img", self.base_dir, storage_id);
        let use_xfs = self.lease_mode == Some(crate::sandbox::TemplateLeaseMode::Shared)
            && self.reflink_supported == Some(true);
        let provider = LocalBlockProvider;
        let lower_dirs = extract_lower_dirs(&m.options);
        let source_identity = lower_dirs.clone().map(|lower| format!("overlay:{}", lower));
        let read_only = m.options.contains(&"ro".to_string());
        let prepare_result = provider
            .prepare(BlockPrepareRequest {
                src_dir: overlay_dir.clone(),
                img_path: img_path.clone(),
                fstype: if use_xfs { BlockFs::Xfs } else { BlockFs::Ext4 },
                fallback_size_mb: self.vm.overlay_image_fallback_size_mb(),
                overhead_percent: self.vm.block_image_size_overhead_percent(),
                source_identity,
                readonly: read_only,
            })
            .await;

        // Step 5: always unmount the host overlay regardless of result
        if let Err(e) = unmount(&overlay_dir, MNT_DETACH | MNT_NOFOLLOW) {
            warn!("failed to unmount overlay {}: {}", overlay_dir, e);
        }
        let _ = tokio::fs::remove_dir_all(&overlay_dir).await;

        let artifact = prepare_result?;

        // Step 6: hot-attach block image as virtio-blk
        let device_id = format!("blk{}", self.increment_and_get_id());
        let attached = BlockAttachTransaction::new(artifact, device_id, read_only)
            .attach(&mut self.vm, &provider)
            .await?;

        // Step 7: record storage entry (need_guest_handle=true, guest mounts via PCI addr)
        let options = if read_only {
            vec!["ro".to_string()]
        } else {
            vec![]
        };
        let mut storage = attached.into_storage(BlockStorageMetadata {
            host_source: m.source.clone(),
            mount_type: m.r#type.clone(),
            storage_id: storage_id.to_string(),
            options,
            mount_point: format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id),
            lower_dirs,
        });
        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }

    async fn handle_bind_mount_blk(
        &mut self,
        storage_id: &str,
        container_id: &str,
        m: &Mount,
        is_rootfs_mount: bool,
    ) -> Result<()> {
        let source = if m.source.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "mount source should exist for bind mount {:?}",
                m
            )));
        } else {
            m.source.clone()
        };

        let read_only = m.options.contains(&"ro".to_string());
        let meta = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|e| anyhow!("stat {}: {}", source, e))?;
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            return Err(Error::InvalidArgument(format!(
                "symlink bind mount source {} is not supported by virtio-blk",
                source
            )));
        }

        if meta.is_file() {
            if is_rootfs_mount {
                return Err(Error::InvalidArgument(format!(
                    "rootfs bind mount {:?} cannot use guest-file in virtio-blk mode",
                    m
                )));
            }
            // Single file: push content to guest via TTRPC exec_vm_process
            let mode = meta.permissions().mode() & 0o777;
            let content = tokio::fs::read(&source)
                .await
                .map_err(|e| anyhow!("read {}: {}", source, e))?;
            let dest_in_guest = join_guest_component(KUASAR_STATE_DIR, storage_id)?;
            let client_guard = self.client.lock().await;
            let client = client_guard.as_ref().ok_or_else(|| {
                anyhow!(
                    "TTRPC client not initialized when pushing file to guest at {}",
                    dest_in_guest
                )
            })?;
            GuestFileInjector::new(client)
                .push_file(&dest_in_guest, content, mode)
                .await?;

            let options = if read_only {
                vec!["ro".to_string()]
            } else {
                vec![]
            };
            let mut storage = Storage {
                host_source: source.clone(),
                r#type: m.r#type.clone(),
                id: storage_id.to_string(),
                device_id: None,
                ref_container: HashMap::new(),
                need_guest_handle: false,
                source: "".to_string(),
                driver: DRIVER_GUEST_FILE.to_string(),
                driver_options: vec![],
                fstype: "bind".to_string(),
                options,
                mount_point: dest_in_guest,
                lower_dirs: None,
                cleanup_path: None,
                owned_by_runtime: false,
                source_identity: Some(format!("bind:{}", source)),
            };
            storage.refer(container_id);
            self.storages.push(storage);
        } else if meta.is_dir() {
            // Directory: scan once to get stats and pre-collect entries for injection.
            let scan = scan_dir(&source).await?;
            if scan.file_count <= self.vm.small_dir_max_files()
                && scan.total_bytes <= self.vm.small_dir_max_bytes()
            {
                if is_rootfs_mount {
                    return Err(Error::InvalidArgument(format!(
                        "rootfs bind mount {:?} cannot use guest-file in virtio-blk mode",
                        m
                    )));
                }
                // Small directory: inject each file via TTRPC (avoids creating an 8MB+ block device)
                let dest_dir_in_guest = join_guest_component(KUASAR_STATE_DIR, storage_id)?;
                self.inject_small_dir(
                    storage_id,
                    container_id,
                    &source,
                    scan,
                    &dest_dir_in_guest,
                    m,
                )
                .await?;
            } else {
                if !self.vm.allow_bind_snapshot() {
                    return Err(Error::InvalidArgument(format!(
                        "large bind mount source {} requires virtio-blk bind snapshot to be explicitly enabled",
                        source
                    )));
                }
                // Large directory (HostPath volumes etc.): block image hot-attached as virtio-blk.
                // Shared mode + reflink_supported uses XFS with reflink=1; otherwise uses ext4.
                let img_path = format!("{}/{}.img", self.base_dir, storage_id);
                let use_xfs = self.lease_mode == Some(crate::sandbox::TemplateLeaseMode::Shared)
                    && self.reflink_supported == Some(true);
                let provider = LocalBlockProvider;
                let artifact = provider
                    .prepare(BlockPrepareRequest {
                        src_dir: source.clone(),
                        img_path: img_path.clone(),
                        fstype: if use_xfs { BlockFs::Xfs } else { BlockFs::Ext4 },
                        fallback_size_mb: self.vm.bind_image_fallback_size_mb(),
                        overhead_percent: self.vm.block_image_size_overhead_percent(),
                        source_identity: Some(format!("bind:{}", source)),
                        readonly: read_only,
                    })
                    .await?;

                let device_id = format!("blk{}", self.increment_and_get_id());
                let attached = BlockAttachTransaction::new(artifact, device_id, read_only)
                    .attach(&mut self.vm, &provider)
                    .await?;

                let options = if read_only {
                    vec!["ro".to_string()]
                } else {
                    vec![]
                };
                let mut storage = attached.into_storage(BlockStorageMetadata {
                    host_source: source.clone(),
                    mount_type: m.r#type.clone(),
                    storage_id: storage_id.to_string(),
                    options,
                    mount_point: format!("{}{}", KUASAR_GUEST_SHARE_DIR, storage_id),
                    lower_dirs: None,
                });
                storage.refer(container_id);
                self.storages.push(storage);
            }
        } else {
            return Err(Error::InvalidArgument(format!(
                "bind mount source {} has unsupported file type for virtio-blk",
                source
            )));
        }
        Ok(())
    }

    // Inject a small directory's files into the guest one by one via TTRPC.
    // Takes a pre-scanned ScannedDir to avoid re-traversing the host directory.
    async fn inject_small_dir(
        &mut self,
        storage_id: &str,
        container_id: &str,
        src_dir: &str,
        scan: ScannedDir,
        dest_dir_in_guest: &str,
        m: &Mount,
    ) -> Result<()> {
        let client_guard = self.client.lock().await;
        let client = client_guard.as_ref().ok_or_else(|| {
            anyhow!(
                "TTRPC client not initialized when injecting directory to guest at {}",
                dest_dir_in_guest
            )
        })?;
        GuestFileInjector::new(client)
            .inject_from_scan(dest_dir_in_guest, &scan)
            .await?;

        let read_only = m.options.contains(&"ro".to_string());
        let options = if read_only {
            vec!["ro".to_string()]
        } else {
            vec![]
        };
        let mut storage = Storage {
            host_source: src_dir.to_string(),
            r#type: m.r#type.clone(),
            id: storage_id.to_string(),
            device_id: None,
            ref_container: HashMap::new(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: DRIVER_GUEST_FILE.to_string(),
            driver_options: vec![],
            fstype: "bind".to_string(),
            options,
            mount_point: dest_dir_in_guest.to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: Some(format!("bind:{}", src_dir)),
        };
        storage.refer(container_id);
        self.storages.push(storage);
        Ok(())
    }
}

/// Extract the `lowerdir=` value from an overlay mount's options list.
/// Returns None if no lowerdir= option is present.
pub(crate) fn extract_lower_dirs(options: &[String]) -> Option<String> {
    options
        .iter()
        .find(|o| o.starts_with("lowerdir="))
        .map(|o| o.trim_start_matches("lowerdir=").to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, os::unix::fs::symlink, path::Path};

    use anyhow::anyhow;
    use async_trait::async_trait;
    use containerd_sandbox::{error::Result, spec::Mount};
    use serde::{Deserialize, Serialize};
    use temp_dir::TempDir;
    use vmm_common::storage::Storage;

    use super::{
        attach::BlockAttachTransaction,
        block_provider::{BlockArtifact, LocalBlockProvider},
        device_graph::DeviceGraph,
        guest_file::{
            join_guest_component, join_guest_path, scan_dir, shell_quote, validate_guest_path,
        },
    };
    use crate::{device::DeviceInfo, sandbox::KuasarSandbox, vm::VM};

    #[derive(Serialize, Deserialize)]
    struct MockVM;

    #[async_trait]
    impl VM for MockVM {
        async fn start(&mut self) -> Result<u32> {
            Ok(0)
        }
        async fn stop(&mut self, _force: bool) -> Result<()> {
            Ok(())
        }
        async fn attach(&mut self, _device_info: DeviceInfo) -> Result<()> {
            Ok(())
        }
        async fn hot_attach(
            &mut self,
            _device_info: DeviceInfo,
        ) -> Result<(crate::device::BusType, String)> {
            Ok((crate::device::BusType::PCI, "/dev/vda".to_string()))
        }
        async fn hot_detach(&mut self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        fn socket_address(&self) -> String {
            "/tmp/mock.sock".to_string()
        }
        async fn wait_channel(&self) -> Option<tokio::sync::watch::Receiver<(u32, i128)>> {
            None
        }
        async fn vcpus(&self) -> Result<crate::vm::VcpuThreads> {
            Ok(crate::vm::VcpuThreads {
                vcpus: HashMap::new(),
            })
        }
        fn pids(&self) -> crate::vm::Pids {
            crate::vm::Pids {
                vmm_pid: None,
                affiliated_pids: vec![],
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct FailingAttachVM;

    #[async_trait]
    impl VM for FailingAttachVM {
        async fn start(&mut self) -> Result<u32> {
            Ok(0)
        }
        async fn stop(&mut self, _force: bool) -> Result<()> {
            Ok(())
        }
        async fn attach(&mut self, _device_info: DeviceInfo) -> Result<()> {
            Ok(())
        }
        async fn hot_attach(
            &mut self,
            _device_info: DeviceInfo,
        ) -> Result<(crate::device::BusType, String)> {
            Err(anyhow!("hot attach failed").into())
        }
        async fn hot_detach(&mut self, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        fn socket_address(&self) -> String {
            "/tmp/mock.sock".to_string()
        }
        async fn wait_channel(&self) -> Option<tokio::sync::watch::Receiver<(u32, i128)>> {
            None
        }
        async fn vcpus(&self) -> Result<crate::vm::VcpuThreads> {
            Ok(crate::vm::VcpuThreads {
                vcpus: HashMap::new(),
            })
        }
        fn pids(&self) -> crate::vm::Pids {
            crate::vm::Pids {
                vmm_pid: None,
                affiliated_pids: vec![],
            }
        }
    }

    #[tokio::test]
    async fn test_block_attach_transaction_releases_artifact_on_hotplug_failure() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("prepared.img");
        tokio::fs::write(&img_path, b"prepared").await.unwrap();
        let artifact = BlockArtifact {
            path: img_path.to_string_lossy().to_string(),
            fstype: "ext4".to_string(),
            readonly: false,
            owned_by_runtime: true,
            cleanup_path: Some(img_path.to_string_lossy().to_string()),
            source_identity: Some("test".to_string()),
        };

        let mut vm = FailingAttachVM;
        let err = BlockAttachTransaction::new(artifact, "blk1".to_string(), false)
            .attach(&mut vm, &LocalBlockProvider)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("hot attach failed"));
        assert!(tokio::fs::metadata(&img_path).await.is_err());
    }

    #[tokio::test]
    async fn test_rootfs_storage_isolation() {
        // We can't easily call attach_storage due to filesystem side-effects (mount, etc.),
        // but we can simulate the logic of its effect and verify deference_storage.
        let mut storages = DeviceGraph::default();
        let mount = Mount {
            r#type: "bind".to_string(),
            source: "/tmp/rootfs".to_string(),
            destination: "/".to_string(),
            options: vec!["ro".to_string()],
        };

        // Simulate attach_storage for container1 (is_rootfs_mount=true)
        let s1 = Storage {
            host_source: mount.source.clone(),
            r#type: mount.r#type.clone(),
            id: "storage1".to_string(),
            device_id: None,
            ref_container: [("container1".to_string(), 1)].into_iter().collect(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: "".to_string(),
            driver_options: vec![],
            fstype: "test".to_string(),
            options: vec!["ro".to_string()],
            mount_point: "/run/kuasar/storage/containers/storage1".to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: None,
        };
        storages.push(s1);

        let mut sandbox = KuasarSandbox {
            vm: MockVM,
            id: "test-sandbox".to_string(),
            status: containerd_sandbox::SandboxStatus::Created,
            base_dir: "/tmp".to_string(),
            data: Default::default(),
            containers: HashMap::new(),
            storages,
            id_generator: 1,
            network: None,
            client: Default::default(),
            exit_signal: Default::default(),
            sandbox_cgroups: Default::default(),
            template_id: None,
            template_snapshot_type: None,
            lease_mode: None,
            reflink_supported: None,
            orphan_container_ids: vec![],
            memory_restore_mode: None,
        };

        // Rootfs mounts skip is_for_mount but still need lower_dirs to match for reuse.
        // storage1 has lower_dirs: None and the bind mount has no lowerdir= option,
        // so no match is expected.
        let reusable = sandbox.find_reusable_storage(&mount, true);
        assert!(reusable.is_none());

        // Validate reuse logic: a regular attach SHOULD reuse existing storage
        let reusable = sandbox.find_reusable_storage(&mount, false);
        assert!(reusable.is_some());
        assert_eq!(reusable.unwrap().id, "storage1");

        // Manually add a second storage for container2 to test deference_storage isolation
        let s2 = Storage {
            host_source: mount.source.clone(),
            r#type: mount.r#type.clone(),
            id: "storage2".to_string(),
            device_id: None,
            ref_container: [("container2".to_string(), 1)].into_iter().collect(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: "".to_string(),
            driver_options: vec![],
            fstype: "test".to_string(),
            options: vec!["ro".to_string()],
            mount_point: "/run/kuasar/storage/containers/storage2".to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: None,
        };
        sandbox.storages.push(s2);

        // Act: deference_storage for container1
        sandbox
            .deference_storage("container1", &mount)
            .await
            .unwrap();

        // Assert: container1's storage should be removed (by GC), container2's should remain.
        assert_eq!(sandbox.storages.len(), 1);
        assert_eq!(sandbox.storages[0].id, "storage2");
        assert!(sandbox.storages[0].ref_container.contains_key("container2"));
    }

    #[test]
    fn test_validate_guest_path_ok() {
        assert!(validate_guest_path("/run/kuasar/state/storage1").is_ok());
        assert!(validate_guest_path("/run/kuasar/storage/containers/storage42").is_ok());
        assert!(validate_guest_path("/etc/resolv.conf").is_ok());
        assert!(validate_guest_path("/tmp/file-name.txt").is_ok());
    }

    #[test]
    fn test_validate_guest_path_reject_shell_special() {
        assert!(validate_guest_path("/tmp/a;b").is_err());
        assert!(validate_guest_path("/tmp/a b").is_err());
        assert!(validate_guest_path("/tmp/$HOME").is_err());
        assert!(validate_guest_path("/tmp/`cmd`").is_err());
        assert!(validate_guest_path("/tmp/a&b").is_err());
        assert!(validate_guest_path("/tmp/a|b").is_err());
    }

    #[test]
    fn test_validate_guest_path_rejects_parent_components() {
        assert!(validate_guest_path("/tmp/../etc/passwd").is_err());
        assert!(validate_guest_path("/tmp/./file").is_err());
        assert!(join_guest_path("/run/kuasar/state/storage1", Path::new("../file")).is_err());
        assert!(join_guest_path("/run/kuasar/state/storage1", Path::new("/abs")).is_err());
        assert_eq!(
            join_guest_path("/run/kuasar/state/storage1", Path::new("sub/file.txt")).unwrap(),
            "/run/kuasar/state/storage1/sub/file.txt"
        );
    }

    #[test]
    fn test_join_guest_component_rejects_path_separators() {
        assert_eq!(
            join_guest_component("/run/kuasar/state", "storage1").unwrap(),
            "/run/kuasar/state/storage1"
        );
        assert!(join_guest_component("/run/kuasar/state", "a/b").is_err());
        assert!(join_guest_component("/run/kuasar/state", "a\\b").is_err());
        assert!(join_guest_component("/run/kuasar/state", "..").is_err());
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(
            shell_quote("/run/kuasar/state/storage1"),
            "'/run/kuasar/state/storage1'"
        );
        assert_eq!(shell_quote("/tmp/file name.txt"), "'/tmp/file name.txt'");
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
        assert_eq!(shell_quote("/tmp/$HOME"), "'/tmp/$HOME'");
    }

    #[tokio::test]
    async fn test_scan_dir_empty() {
        let dir = TempDir::new().unwrap();
        let scan = scan_dir(dir.path().to_str().unwrap()).await.unwrap();
        assert_eq!(scan.file_count, 0);
        assert_eq!(scan.total_bytes, 0);
    }

    #[tokio::test]
    async fn test_scan_dir_with_files() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"hello")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"world!")
            .await
            .unwrap();
        let sub = dir.path().join("sub");
        tokio::fs::create_dir(&sub).await.unwrap();
        tokio::fs::write(sub.join("c.txt"), b"123").await.unwrap();

        let scan = scan_dir(dir.path().to_str().unwrap()).await.unwrap();
        assert_eq!(scan.file_count, 3);
        assert_eq!(scan.total_bytes, 5 + 6 + 3);
    }

    #[tokio::test]
    async fn test_scan_dir_counts_symlink_as_entry() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.txt");
        tokio::fs::write(&target, b"hello").await.unwrap();
        symlink(&target, dir.path().join("link.txt")).unwrap();

        // Symlinks are counted toward the threshold but contribute 0 bytes.
        // inject_from_scan rejects them at injection time.
        let scan = scan_dir(dir.path().to_str().unwrap()).await.unwrap();
        assert_eq!(scan.file_count, 2); // target.txt + link.txt
        assert_eq!(scan.total_bytes, 5); // only target.txt's bytes
    }

    #[tokio::test]
    async fn test_scan_dir_rejects_special_files() {
        let dir = TempDir::new().unwrap();
        let fifo_path = dir.path().join("test.fifo");
        // Create a named pipe (FIFO) — a special file unsupported by virtio-blk
        let status = std::process::Command::new("mkfifo")
            .arg(fifo_path.to_str().unwrap())
            .status()
            .expect("mkfifo must be available");
        assert!(status.success(), "mkfifo failed");

        let err = scan_dir(dir.path().to_str().unwrap()).await.unwrap_err();
        assert!(
            err.to_string().contains("not supported by virtio-blk"),
            "expected virtio-blk rejection, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_detach_storage_not_found() {
        let mut sandbox = KuasarSandbox {
            vm: MockVM,
            id: "test-sandbox".to_string(),
            status: containerd_sandbox::SandboxStatus::Created,
            base_dir: "/tmp/non-existent-dir-12345".to_string(),
            data: Default::default(),
            containers: HashMap::new(),
            storages: DeviceGraph::default(),
            id_generator: 1,
            network: None,
            client: Default::default(),
            exit_signal: Default::default(),
            sandbox_cgroups: Default::default(),
            template_id: None,
            template_snapshot_type: None,
            lease_mode: None,
            reflink_supported: None,
            orphan_container_ids: vec![],
            memory_restore_mode: None,
        };

        let storage = Storage {
            host_source: "".to_string(),
            r#type: "bind".to_string(),
            id: "storage1".to_string(),
            device_id: Some("blk1".to_string()),
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: "/dev/vda".to_string(),
            driver: "blk".to_string(),
            driver_options: vec![],
            fstype: "ext4".to_string(),
            options: vec![],
            mount_point: "/run/kuasar/storage/containers/storage1".to_string(),
            lower_dirs: None,
            cleanup_path: Some("/tmp/non-existent-kuasar-storage.img".to_string()),
            owned_by_runtime: true,
            source_identity: None,
        };

        // This should not fail even if the runtime-owned cleanup path is gone.
        sandbox.detach_storage(&storage).await.unwrap();
    }

    #[tokio::test]
    async fn test_detach_external_ext4_storage_does_not_remove_file() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("external.img");
        tokio::fs::write(&img_path, b"external").await.unwrap();

        let mut sandbox = KuasarSandbox {
            vm: MockVM,
            id: "test-sandbox".to_string(),
            status: containerd_sandbox::SandboxStatus::Created,
            base_dir: dir.path().to_string_lossy().to_string(),
            data: Default::default(),
            containers: HashMap::new(),
            storages: DeviceGraph::default(),
            id_generator: 1,
            network: None,
            client: Default::default(),
            exit_signal: Default::default(),
            sandbox_cgroups: Default::default(),
            template_id: None,
            template_snapshot_type: None,
            lease_mode: None,
            reflink_supported: None,
            orphan_container_ids: vec![],
            memory_restore_mode: None,
        };

        let storage = Storage {
            host_source: img_path.to_string_lossy().to_string(),
            r#type: "bind".to_string(),
            id: "storage1".to_string(),
            device_id: Some("blk1".to_string()),
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: "/dev/vda".to_string(),
            driver: "blk".to_string(),
            driver_options: vec![],
            fstype: "ext4".to_string(),
            options: vec![],
            mount_point: "/run/kuasar/storage/containers/storage1".to_string(),
            lower_dirs: None,
            cleanup_path: Some(img_path.to_string_lossy().to_string()),
            owned_by_runtime: false,
            source_identity: None,
        };

        sandbox.detach_storage(&storage).await.unwrap();
        assert!(tokio::fs::metadata(&img_path).await.is_ok());
    }

    #[tokio::test]
    async fn test_attach_storage_adoption_failure_is_fatal() {
        let mount = Mount {
            r#type: "bind".to_string(),
            source: "/tmp/orphan-rootfs".to_string(),
            destination: "/data".to_string(),
            options: vec![],
        };
        let storage = Storage {
            host_source: mount.source.clone(),
            r#type: mount.r#type.clone(),
            id: "storage1".to_string(),
            device_id: None,
            ref_container: [("old-container".to_string(), 1)].into_iter().collect(),
            need_guest_handle: false,
            source: "".to_string(),
            driver: "".to_string(),
            driver_options: vec![],
            fstype: "bind".to_string(),
            options: vec![],
            mount_point: "/run/kuasar/state/storage1".to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: None,
        };

        let mut sandbox = KuasarSandbox {
            vm: MockVM,
            id: "test-sandbox".to_string(),
            status: containerd_sandbox::SandboxStatus::Running(123),
            base_dir: "/tmp".to_string(),
            data: Default::default(),
            containers: HashMap::new(),
            storages: vec![storage].into(),
            id_generator: 1,
            network: None,
            client: Default::default(),
            exit_signal: Default::default(),
            sandbox_cgroups: Default::default(),
            template_id: None,
            template_snapshot_type: None,
            lease_mode: None,
            reflink_supported: None,
            orphan_container_ids: vec!["old-container".to_string()],
            memory_restore_mode: None,
        };

        let err = sandbox
            .attach_storage("new-container", &mount, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to co-run"));
        assert!(sandbox.storages[0]
            .ref_container
            .contains_key("old-container"));
        assert!(!sandbox.storages[0]
            .ref_container
            .contains_key("new-container"));
    }

    #[tokio::test]
    async fn test_estimate_dir_size_mb() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("file.txt"), b"x".repeat(1024))
            .await
            .unwrap();

        let size_mb = super::block_provider::estimate_dir_size_mb(dir.path().to_str().unwrap())
            .await
            .unwrap();
        // du -sm reports in MB, should be at least 1 for a 1KB file (due to block size)
        assert!(size_mb >= 1);
    }

    #[tokio::test]
    async fn test_small_dir_threshold_logic() {
        let small_dir = TempDir::new().unwrap();
        tokio::fs::write(small_dir.path().join("a.txt"), b"hello")
            .await
            .unwrap();
        tokio::fs::write(small_dir.path().join("b.txt"), b"world")
            .await
            .unwrap();
        let scan = scan_dir(small_dir.path().to_str().unwrap()).await.unwrap();
        assert!(scan.file_count <= crate::vm::DEFAULT_SMALL_DIR_MAX_FILES);
        assert!(scan.total_bytes <= crate::vm::DEFAULT_SMALL_DIR_MAX_BYTES);
    }

    #[tokio::test]
    async fn test_large_dir_threshold_logic() {
        let large_dir = TempDir::new().unwrap();
        for i in 0..100 {
            tokio::fs::write(
                large_dir.path().join(format!("file{}.txt", i)),
                b"x".repeat(1024),
            )
            .await
            .unwrap();
        }
        let scan = scan_dir(large_dir.path().to_str().unwrap()).await.unwrap();
        assert!(
            scan.file_count > crate::vm::DEFAULT_SMALL_DIR_MAX_FILES
                || scan.total_bytes > crate::vm::DEFAULT_SMALL_DIR_MAX_BYTES
        );
    }

    #[tokio::test]
    #[ignore = "requires root (mkfs.ext4)"]
    async fn test_create_ext4_image_integration() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("test.img");
        let img_path_str = img_path.to_str().unwrap();

        // Create a small ext4 image
        super::block_provider::create_ext4_image_with_inodes(img_path_str, 8, 1024)
            .await
            .unwrap();

        // Verify the image was created
        let meta = tokio::fs::metadata(&img_path).await.unwrap();
        assert!(meta.len() >= 8 * 1024 * 1024);

        // Verify it's a valid ext4 filesystem by checking with blkid or file command
        let output = tokio::process::Command::new("file")
            .arg(img_path_str)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("ext4") || stdout.contains("Linux rev 1.0 ext4"));
    }

    #[tokio::test]
    #[ignore = "requires root (mount -o loop)"]
    async fn test_copy_dir_to_ext4_integration() {
        let dir = TempDir::new().unwrap();

        // Create source directory with content
        let src_dir = dir.path().join("src");
        tokio::fs::create_dir_all(&src_dir).await.unwrap();
        tokio::fs::write(src_dir.join("test.txt"), b"hello world")
            .await
            .unwrap();
        let nested_dir = src_dir.join("nested");
        tokio::fs::create_dir_all(&nested_dir).await.unwrap();
        tokio::fs::write(nested_dir.join("data.bin"), b"binary data")
            .await
            .unwrap();

        // Create ext4 image and copy content
        let img_path = dir.path().join("test.img");
        let img_path_str = img_path.to_str().unwrap();
        super::block_provider::create_ext4_image_with_inodes(img_path_str, 16, 1024)
            .await
            .unwrap();
        super::block_provider::copy_dir_to_ext4(src_dir.to_str().unwrap(), img_path_str)
            .await
            .unwrap();

        // Mount, verify content, then unmount — collect results before asserting so that
        // cleanup always runs even when assertions fail.
        let mnt_dir = dir.path().join("mnt");
        tokio::fs::create_dir_all(&mnt_dir).await.unwrap();
        let mount_ok = tokio::process::Command::new("mount")
            .args(["-o", "loop", img_path_str, mnt_dir.to_str().unwrap()])
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        let test_txt_ok = if mount_ok {
            tokio::fs::metadata(mnt_dir.join("test.txt")).await.is_ok()
        } else {
            false
        };
        let data_bin_ok = if mount_ok {
            tokio::fs::metadata(mnt_dir.join("nested/data.bin"))
                .await
                .is_ok()
        } else {
            false
        };

        // Cleanup before asserting so umount runs even when assertions would fail
        if mount_ok {
            let _ = tokio::process::Command::new("umount")
                .arg(mnt_dir.to_str().unwrap())
                .status()
                .await;
        }

        assert!(mount_ok, "loop-mount of ext4 image failed");
        assert!(test_txt_ok, "test.txt not found in ext4 image");
        assert!(data_bin_ok, "nested/data.bin not found in ext4 image");
    }
}

pub struct MountInfo {
    pub mount_point: String,
    pub fs_type: String,
    pub options: Vec<String>,
}
