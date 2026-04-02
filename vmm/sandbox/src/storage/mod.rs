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

use std::{collections::HashMap, io::ErrorKind, path::Path};

use anyhow::anyhow;
use containerd_sandbox::{
    error::{Error, Result},
    spec::Mount,
};
use containerd_shim::mount::mount_rootfs;
use log::debug;
use nix::libc::MNT_DETACH;
pub use utils::*;
use vmm_common::{
    mount::{bind_mount, unmount, MNT_NOFOLLOW},
    storage::{Storage, DRIVEREPHEMERALTYPE},
    KUASAR_STATE_DIR,
};

use crate::{
    device::{BlockDeviceInfo, DeviceInfo},
    sandbox::{KuasarSandbox, KUASAR_GUEST_SHARE_DIR},
    storage::mount::{get_mount_info, is_bind, is_bind_shm, is_overlay},
    vm::{BlockDriver, VM},
};

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
            self.handle_bind_mount(&id, container_id, m).await?;
            return Ok(());
        }

        if is_overlay(m) {
            self.handle_overlay_mount(&id, container_id, m).await?;
            return Ok(());
        }

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

    fn find_reusable_storage(&mut self, m: &Mount, is_rootfs_mount: bool) -> Option<&mut Storage> {
        if is_rootfs_mount {
            return None;
        }
        self.storages.iter_mut().find(|s| s.is_for_mount(m))
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
        let storage_infos: Vec<(Option<String>, String, String)> = self
            .storages
            .iter()
            .filter(|&x| x.ref_count() == 0)
            .map(|s| (s.device_id.clone(), s.id.clone(), s.fstype.clone()))
            .collect();
        for info in storage_infos {
            self.detach_storage(info.0.clone(), &info.1, &info.2)
                .await?;
            self.storages.retain(|x| x.id != info.1);
        }
        Ok(())
    }

    async fn detach_storage(
        &mut self,
        device_id: Option<String>,
        id: &str,
        fs_type: &str,
    ) -> Result<()> {
        if device_id.is_some() {
            self.vm.hot_detach(&device_id.unwrap()).await?;
        } else if fs_type == "bind" {
            let mount_point = format!("{}/{}", self.get_sandbox_shared_path(), &id);
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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use containerd_sandbox::{error::Result, spec::Mount};
    use serde::{Deserialize, Serialize};
    use vmm_common::storage::Storage;

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

    #[tokio::test]
    async fn test_rootfs_storage_isolation() {
        // We can't easily call attach_storage due to filesystem side-effects (mount, etc.),
        // but we can simulate the logic of its effect and verify deference_storage.
        let mut storages = vec![];
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
        };

        // Validate reuse logic: a second rootfs attach should NOT reuse existing storage
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

    #[tokio::test]
    async fn test_detach_storage_not_found() {
        let mut sandbox = KuasarSandbox {
            vm: MockVM,
            id: "test-sandbox".to_string(),
            status: containerd_sandbox::SandboxStatus::Created,
            base_dir: "/tmp/non-existent-dir-12345".to_string(),
            data: Default::default(),
            containers: HashMap::new(),
            storages: vec![],
            id_generator: 1,
            network: None,
            client: Default::default(),
            exit_signal: Default::default(),
            sandbox_cgroups: Default::default(),
        };

        // This should not fail even if the directory doesn't exist
        sandbox
            .detach_storage(None, "storage1", "bind")
            .await
            .unwrap();
    }
}

pub struct MountInfo {
    pub mount_point: String,
    pub fs_type: String,
    pub options: Vec<String>,
}
