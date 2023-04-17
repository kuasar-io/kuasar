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

use std::{path::Path, time::Duration};

use containerd_shim::{error::Error, other, other_error, util::IntoOption, Result};
use log::{debug, warn};
use vmm_common::{
    mount::{mount, unmount},
    storage::{Storage, DRIVERBLKTYPE, DRIVEREPHEMERALTYPE, DRIVERSCSITYPE},
};

use crate::device::{scan_scsi_bus, Device, DeviceMatcher, DeviceMonitor, DeviceType};

pub struct SandboxResources {
    storages: Vec<Storage>,
    device_monitor: DeviceMonitor,
}

impl SandboxResources {
    pub async fn new() -> Self {
        let device_monitor = DeviceMonitor::new();
        device_monitor.start().await;
        Self {
            storages: vec![],
            device_monitor,
        }
    }

    pub async fn add_storages(&mut self, container_id: &str, storages: Vec<Storage>) -> Result<()> {
        for s in storages {
            self.add_storage(container_id, s).await?;
        }
        Ok(())
    }

    pub async fn defer_storages(&mut self, container_id: &str) -> Result<()> {
        for s in &mut self.storages {
            s.defer(container_id);
        }
        self.gc_storages().await?;
        Ok(())
    }

    pub async fn add_storage(&mut self, container_id: &str, mut storage: Storage) -> Result<()> {
        for s in &mut self.storages {
            if s.host_source == storage.host_source && s.r#type == storage.r#type {
                s.refer(container_id);
                return Ok(());
            }
        }

        match &*storage.driver {
            DRIVERSCSITYPE => {
                self.handle_scsi_storage(&mut storage).await?;
            }
            DRIVEREPHEMERALTYPE => {
                mount_storage(&storage).await?;
            }
            DRIVERBLKTYPE => {
                self.handle_blk_storage(&mut storage).await?;
            }
            _ => {
                unimplemented!("storage driver not implemented {}", storage.driver)
            }
        }
        self.storages.push(storage);
        Ok(())
    }

    async fn gc_storages(&mut self) -> Result<()> {
        let mut removed = vec![];
        self.storages.retain(|x| {
            if x.ref_count() == 0 {
                removed.push(x.clone());
                false
            } else {
                true
            }
        });
        for s in removed {
            debug!("unmount storage {:?}", s);
            if let Err(_e) = unmount_storage(&s).await {
                warn!("failed to unmount storage {:?}", s);
            }
        }
        let _mounts = tokio::fs::read_to_string("/proc/mounts")
            .await
            .unwrap_or_default();
        Ok(())
    }

    async fn handle_scsi_storage(&mut self, storage: &mut Storage) -> Result<()> {
        scan_scsi_bus(&storage.source).await?;
        // Retrieve the device path from SCSI address.
        let device = self.get_device(&storage.source, DeviceType::Scsi).await?;
        let path = device.path.to_string();
        storage.source = path;

        mount_storage(storage).await?;
        Ok(())
    }

    async fn handle_blk_storage(&mut self, storage: &mut Storage) -> Result<()> {
        // Retrieve the device path from pci address.
        let device = self.get_device(&storage.source, DeviceType::Blk).await?;
        let path = device.path.to_string();
        storage.source = path;

        mount_storage(storage).await?;
        Ok(())
    }

    async fn get_device(&self, addr: &str, ty: DeviceType) -> Result<Device> {
        let mut s = self
            .device_monitor
            .subscribe(TypeAddrDeviceMatcher::new(ty, addr.to_string()))
            .await;
        let res = tokio::time::timeout(Duration::from_secs(10), s.rx.recv())
            .await
            .map_err(other_error!(
                e,
                format!("timeout waiting for device with addr {} ready", addr)
            ))?;
        res.ok_or_else(|| other!("can not get device with addr {}", addr))
    }
}

async fn mount_storage(storage: &Storage) -> Result<()> {
    let src_path = Path::new(&storage.source);
    if storage.fstype == "bind" && !src_path.is_dir() {
        ensure_destination_file_exists(Path::new(&storage.mount_point)).await?
    } else {
        tokio::fs::create_dir_all(&storage.mount_point)
            .await
            .map_err(other_error!(
                e,
                format!("failed to create dir {}", storage.mount_point)
            ))?;
    }

    debug!("mounting storage {:?}", storage);
    let fstype = storage.fstype.as_str().none_if(|x| x.is_empty());
    let source = storage.source.as_str().none_if(|x| x.is_empty());
    mount(fstype, source, &storage.options, &storage.mount_point).map_err(other_error!(e, ""))?;
    Ok(())
}

async fn unmount_storage(storage: &Storage) -> Result<()> {
    let src_path = Path::new(&storage.source);
    unmount(&storage.mount_point, 0).map_err(other_error!(e, ""))?;
    if storage.fstype == "bind" && !src_path.is_dir() {
        tokio::fs::remove_file(&storage.mount_point)
            .await
            .map_err(other_error!(e, ""))?
    } else {
        tokio::fs::remove_dir(&storage.mount_point)
            .await
            .map_err(other_error!(e, ""))?
    }
    Ok(())
}

async fn ensure_destination_file_exists(path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    } else if path.exists() {
        return Err(other!("{:?} exists but is not a regular file", path));
    }

    let dir = path
        .parent()
        .ok_or_else(|| other!("failed to find parent path for {:?}", path))?;

    tokio::fs::create_dir_all(dir).await.map_err(other_error!(
        e,
        format!("failed to create {}", dir.display())
    ))?;

    tokio::fs::File::create(path).await.map_err(other_error!(
        e,
        format!("failed to create file {}", path.display())
    ))?;

    Ok(())
}

pub struct TypeAddrDeviceMatcher {
    addr: String,
    r#type: DeviceType,
}

impl TypeAddrDeviceMatcher {
    pub fn new(r#type: DeviceType, addr: String) -> Self {
        Self { addr, r#type }
    }
}

impl DeviceMatcher for TypeAddrDeviceMatcher {
    fn is_match(&self, device: &Device) -> bool {
        if self.addr == device.addr && self.r#type == device.r#type {
            return true;
        }
        false
    }
}
