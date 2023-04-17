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

use async_trait::async_trait;
use containerd_sandbox::error::Result;
use log::{debug, error};
use qapi::{
    qmp::{
        blockdev_add, blockdev_del, device_add, device_del, BlockdevOptions, BlockdevOptionsBase,
        BlockdevOptionsFile,
    },
    Dictionary,
};
use sandbox_derive::CmdLineParams;
use serde_json::Value;

use crate::{
    device::{BusType, Device, Transport},
    qemu::{devices::HotAttachable, qmp_client::QmpClient},
};

pub const VIRTIO_BLK_DRIVER: &str = "virtio-blk";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device", "drive")]
pub struct VirtioBlockDevice {
    #[property(param = "device", ignore_key)]
    pub driver: String,
    #[property(param = "drive")]
    #[property(param = "device", key = "drive")]
    pub id: String,
    #[property(param = "drive")]
    pub file: Option<String>,
    #[property(param = "drive")]
    pub r#if: Option<String>,
    #[property(param = "drive")]
    pub aio: Option<String>,
    #[property(param = "drive")]
    pub format: Option<String>,
    #[property(param = "device", generator = "crate::utils::bool_to_on_off")]
    pub scsi: Option<bool>,
    #[property(
        param = "device",
        key = "config-wce",
        generator = "crate::utils::bool_to_on_off"
    )]
    pub wce: Option<bool>,
    #[property(param = "device")]
    pub disable_modern: Option<bool>,
    #[property(param = "device")]
    pub romfile: Option<String>,
    #[property(
        param = "device",
        predicate = "self.share_rw==true",
        generator = "crate::utils::bool_to_on_off"
    )]
    pub share_rw: bool,
    #[property(param = "drive", generator = "crate::utils::bool_to_on_off")]
    pub readonly: bool,
}

impl_device_no_bus!(VirtioBlockDevice);

impl VirtioBlockDevice {
    pub fn new(driver: &str, id: &str, file: Option<String>, read_only: bool) -> Self {
        Self {
            driver: driver.to_string(),
            id: id.to_string(),
            file,
            r#if: None,
            aio: None,
            format: None,
            scsi: Some(false),
            wce: Some(false),
            disable_modern: None,
            romfile: None,
            share_rw: false,
            readonly: read_only,
        }
    }
}

#[async_trait]
impl HotAttachable for VirtioBlockDevice {
    async fn execute_hot_attach(
        &self,
        client: &QmpClient,
        bus_type: &BusType,
        bus_id: &str,
        slot_index: usize,
    ) -> Result<()> {
        debug!("hot attach block device {}", self.id);
        client.execute(self.to_blockdev_add()).await?;
        match client
            .execute(self.to_device_add(bus_type, bus_id, slot_index))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                client
                    .execute(self.to_blockdev_del())
                    .await
                    .unwrap_or_else(|e| {
                        error!("failed to delete blockdev after device_add failed, {:?}", e);
                        qapi::Empty {}
                    });
                Err(e)
            }
        }
    }

    async fn execute_hot_detach(&self, client: &QmpClient) -> Result<()> {
        debug!("hot detach device {}", self.id);
        let device_id = format!("virtio-{}", self.id());
        client.delete_device(&device_id).await?;
        client.execute(self.to_blockdev_del()).await?;
        Ok(())
    }
}

impl VirtioBlockDevice {
    fn to_blockdev_add(&self) -> blockdev_add {
        blockdev_add(BlockdevOptions::host_device {
            base: BlockdevOptionsBase {
                node_name: Some(self.id.to_string()),
                read_only: Some(self.readonly),
                auto_read_only: None,
                cache: None,
                force_share: None,
                discard: None,
                detect_zeroes: None,
            },
            host_device: BlockdevOptionsFile {
                drop_cache: None,
                locking: None,
                x_check_cache_dropped: None,
                filename: self.file.as_ref().unwrap().to_string(),
                aio: None,
                pr_manager: None,
            },
        })
    }

    fn to_device_add(&self, bus_type: &BusType, bus_id: &str, index: usize) -> device_add {
        let mut args = Dictionary::new();
        if self.share_rw {
            args.insert(
                "share-rw".to_string(),
                Value::from(crate::utils::bool_to_on_off(&self.share_rw)),
            );
        }
        args.insert("drive".to_string(), Value::from(self.id()));
        if let BusType::SCSI = bus_type {
            args.insert("scsi-id".to_string(), Value::from(index / 256));
            args.insert("lun".to_string(), Value::from(index % 256));
        } else {
            let addr = format!("{:02x}", index);
            args.insert("addr".to_string(), Value::from(addr));
        }
        if let Some(x) = self.disable_modern {
            args.insert("disable-modern".to_string(), Value::from(x));
        }
        if let Some(x) = self.romfile.as_ref() {
            args.insert("romfile".to_string(), Value::from(x.to_string()));
        }
        let driver = match bus_type {
            BusType::PCI => Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
            BusType::PCIE => Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
            BusType::CCW => Transport::Ccw.to_driver(VIRTIO_BLK_DRIVER),
            BusType::SCSI => "scsi-hd".to_string(),
            _ => Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
        };

        let bus = if let BusType::SCSI = bus_type {
            // virtio-scsi bus option is like this
            Some(format!("{}.0", bus_id))
        } else {
            Some(bus_id.to_string())
        };

        device_add {
            driver,
            bus,
            id: Some(format!("virtio-{}", self.id())),
            arguments: args,
        }
    }

    #[allow(dead_code)]
    fn to_device_del(&self) -> device_del {
        device_del {
            id: format!("virtio-{}", self.id()),
        }
    }

    fn to_blockdev_del(&self) -> blockdev_del {
        blockdev_del {
            node_name: self.id.to_string(),
        }
    }
}
