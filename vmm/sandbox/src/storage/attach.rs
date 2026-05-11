/*
Copyright 2026 The Kuasar Authors.

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

use containerd_sandbox::error::Result;
use log::warn;
use vmm_common::storage::Storage;

use crate::{
    device::{BlockDeviceInfo, BusType, DeviceInfo},
    storage::block_provider::{BlockArtifact, BlockProvider},
    vm::{BlockDriver, VM},
};

pub(crate) struct BlockAttachTransaction {
    artifact: BlockArtifact,
    device_id: String,
    read_only: bool,
}

#[derive(Debug)]
pub(crate) struct AttachedBlock {
    artifact: BlockArtifact,
    device_id: String,
    bus_type: BusType,
    guest_addr: String,
}

pub(crate) struct BlockStorageMetadata {
    pub(crate) host_source: String,
    pub(crate) mount_type: String,
    pub(crate) storage_id: String,
    pub(crate) options: Vec<String>,
    pub(crate) mount_point: String,
    pub(crate) lower_dirs: Option<String>,
}

impl BlockAttachTransaction {
    pub(crate) fn new(artifact: BlockArtifact, device_id: String, read_only: bool) -> Self {
        Self {
            artifact,
            device_id,
            read_only,
        }
    }

    pub(crate) async fn attach<V, P>(self, vm: &mut V, provider: &P) -> Result<AttachedBlock>
    where
        V: VM + Sync + Send,
        P: BlockProvider + Sync,
    {
        let hot_result = vm
            .hot_attach(DeviceInfo::Block(BlockDeviceInfo {
                id: self.device_id.clone(),
                path: self.artifact.path.clone(),
                read_only: self.read_only,
            }))
            .await;

        match hot_result {
            Ok((bus_type, guest_addr)) => Ok(AttachedBlock {
                artifact: self.artifact,
                device_id: self.device_id,
                bus_type,
                guest_addr,
            }),
            Err(e) => {
                let _ = provider.release(&self.artifact).await;
                Err(e)
            }
        }
    }
}

impl AttachedBlock {
    /// Rollback a successful attach when a later step fails.
    ///
    /// Detaches the hypervisor block device and releases the artifact if it is
    /// runtime-owned.  Errors are logged but not re-raised so this is always safe
    /// to call on an error path.
    ///
    /// NOTE: this covers the host-side transaction steps only.  The proposal also
    /// specifies a guest-side "verify" step (guest waits for uevent, resolves BDF →
    /// /dev/vdX, mounts and confirms fstype/readonly).  That step is not yet
    /// implemented here; it requires a dedicated guest confirmation RPC.
    #[allow(dead_code)]
    pub(crate) async fn rollback<V, P>(self, vm: &mut V, provider: &P)
    where
        V: VM + Sync + Send,
        P: BlockProvider + Sync,
    {
        if let Err(e) = vm.hot_detach(&self.device_id).await {
            warn!("rollback: hot_detach {} failed: {}", self.device_id, e);
        }
        if let Err(e) = provider.release(&self.artifact).await {
            warn!(
                "rollback: release artifact {} failed: {}",
                self.artifact.path, e
            );
        }
    }

    pub(crate) fn into_storage(self, meta: BlockStorageMetadata) -> Storage {
        Storage {
            host_source: meta.host_source,
            r#type: meta.mount_type,
            id: meta.storage_id,
            device_id: Some(self.device_id),
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: self.guest_addr,
            driver: BlockDriver::from_bus_type(&self.bus_type).to_driver_string(),
            driver_options: vec![],
            fstype: self.artifact.fstype,
            options: meta.options,
            mount_point: meta.mount_point,
            lower_dirs: meta.lower_dirs,
            cleanup_path: self.artifact.cleanup_path,
            owned_by_runtime: self.artifact.owned_by_runtime,
            source_identity: self.artifact.source_identity,
        }
    }
}
