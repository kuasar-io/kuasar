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
use vmm_common::{storage::ANNOTATION_KEY_STORAGE, DEV_SHM, STORAGE_FILE_PREFIX};

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
        let mut storages = vec![];

        for mut m in mounts {
            if let Some(storage) = sandbox.storages.iter().find(|x| x.is_for_mount(&m)) {
                debug!("found storage {:?} for mount {:?}", storage, m);
                m.source = storage.mount_point.clone();
                m.options = vec!["bind".to_string()];
                if storage.need_guest_handle {
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
            if storage.need_guest_handle {
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
