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

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    error::{Error, Result},
    Sandbox,
};
use log::debug;
use vmm_common::{
    storage::{Storage, ANNOTATION_KEY_STORAGE},
    DEV_SHM, STORAGE_FILE_PREFIX,
};

use crate::{
    container::handler::Handler, sandbox::KuasarSandbox, storage::mount::is_bind_shm,
    utils::write_file_atomic, vm::VM,
};

pub struct StorageHandler {
    container_id: String,
}

impl StorageHandler {
    pub fn new(container_id: &str) -> Self {
        Self {
            container_id: container_id.to_string(),
        }
    }
}

fn contains_guest_storage(storages: &[&Storage], storage: &Storage) -> bool {
    storages.iter().any(|s| {
        s.host_source == storage.host_source
            && s.r#type == storage.r#type
            && s.mount_point == storage.mount_point
    })
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for StorageHandler
where
    T: VM + Sync + Send,
{
    async fn handle(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let container = sandbox.container(&self.container_id).await?;
        let mounts = if let Some(c) = &container.data.spec {
            c.mounts.clone()
        } else {
            vec![]
        };
        let rootfs = &container.data.rootfs;

        let mut handled_mounts = vec![];
        let mut storages: Vec<&Storage> = vec![];

        for mut m in mounts {
            if let Some(storage) = sandbox.storages.iter().find(|x| x.is_for_mount(&m)) {
                debug!("found storage {:?} for mount {:?}", storage, m);
                m.source.clone_from(&storage.mount_point);
                m.options.push("bind".to_string());
                if storage.need_guest_handle && !contains_guest_storage(&storages, storage) {
                    storages.push(storage);
                }
            }
            // TODO if vmm-task mount shm when startup, then just use the same shm
            if is_bind_shm(&m) {
                m.source = DEV_SHM.to_string();
                m.options.push("rbind".to_string());
            }
            handled_mounts.push(m);
        }

        let mut root_source = "rootfs".to_string();
        for m in rootfs {
            let storage = sandbox
                .storages
                .iter()
                .find(|x| x.is_for_mount(m))
                .ok_or_else(|| {
                    Error::NotFound(format!(
                        "can not find storage of rootfs for container {}",
                        &self.container_id
                    ))
                })?;
            root_source = storage.mount_point.to_string();
            if storage.need_guest_handle && !contains_guest_storage(&storages, storage) {
                storages.push(storage);
            }
        }

        let storage_str = serde_json::to_string(&storages)
            .map_err(|e| anyhow!("failed to parse storages {}", e))?;

        let container = sandbox.container_mut(&self.container_id)?;
        if let Some(spec) = &mut container.data.spec {
            spec.mounts = handled_mounts;
            spec.annotations
                .insert(ANNOTATION_KEY_STORAGE.to_string(), storage_str.clone());
            // make root the rootfs mountpoint in vm directly, and remove all rootfs mounts.
            // because the storage is handled, then no need to do any rootfs mounts anymore.
            if let Some(x) = spec.root.as_mut() {
                x.path = root_source;
            }
        };
        let storage_file_path = format!(
            "{}/{}-{}",
            container.data.bundle, STORAGE_FILE_PREFIX, self.container_id
        );
        write_file_atomic(&storage_file_path, &storage_str).await?;

        container.data.rootfs = vec![];
        Ok(())
    }

    async fn rollback(&self, sandbox: &mut KuasarSandbox<T>) -> Result<()> {
        let container = sandbox.container(&self.container_id).await?;
        let storage_file_path = format!(
            "{}/{}-{}",
            container.data.bundle, STORAGE_FILE_PREFIX, self.container_id
        );
        tokio::fs::remove_file(&storage_file_path)
            .await
            .unwrap_or_default();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::storage::guest_file::DRIVER_GUEST_FILE;

    fn storage(host_source: &str, r#type: &str, mount_point: &str) -> Storage {
        Storage {
            host_source: host_source.to_string(),
            r#type: r#type.to_string(),
            id: String::new(),
            device_id: None,
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: String::new(),
            driver: String::new(),
            driver_options: vec![],
            fstype: String::new(),
            options: vec![],
            mount_point: mount_point.to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: None,
        }
    }

    #[test]
    fn contains_guest_storage_includes_mount_point_in_identity() {
        let first = storage("/host/layer", "overlay", "/run/kuasar/storage/a");
        let second = storage("/host/layer", "overlay", "/run/kuasar/storage/b");
        let storages = vec![&first];

        assert!(!contains_guest_storage(&storages, &second));
    }

    #[test]
    fn contains_guest_storage_matches_same_storage_identity() {
        let first = storage("/host/layer", "overlay", "/run/kuasar/storage/a");
        let same = storage("/host/layer", "overlay", "/run/kuasar/storage/a");
        let storages = vec![&first];

        assert!(contains_guest_storage(&storages, &same));
    }

    #[test]
    fn guest_file_storage_excluded_from_annotation() {
        // guest-file storages have need_guest_handle=false so they are never sent
        // to the guest in the storage annotation — the guest-file injection already
        // delivered the content directly via ExecVMProcess.
        let s = Storage {
            host_source: "/host/file".to_string(),
            r#type: "bind".to_string(),
            id: "guestfile-1".to_string(),
            device_id: None,
            ref_container: HashMap::new(),
            need_guest_handle: false,
            source: String::new(),
            driver: DRIVER_GUEST_FILE.to_string(),
            driver_options: vec![],
            fstype: String::new(),
            options: vec![],
            mount_point: "/run/kuasar/storage/containers/guestfile-1".to_string(),
            lower_dirs: None,
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: None,
        };

        // Simulate StorageHandler::handle()'s annotation filter
        let mut annotation_storages: Vec<&Storage> = vec![];
        if s.need_guest_handle && !contains_guest_storage(&annotation_storages, &s) {
            annotation_storages.push(&s);
        }

        assert!(
            annotation_storages.is_empty(),
            "guest-file storage must not appear in the storage annotation"
        );
    }
}
