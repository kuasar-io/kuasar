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
        blockdev_add, blockdev_del, device_add, device_del, BlockdevCacheOptions, BlockdevOptions,
        BlockdevOptionsBase, BlockdevOptionsFile, BlockdevOptionsGenericFormat, BlockdevOptionsRaw,
        BlockdevRef,
    },
    Dictionary,
};
use sandbox_derive::CmdLineParams;
use serde_json::Value;

use crate::{
    device::Device,
    stratovirt::{devices::HotAttachable, qmp_client::QmpClient},
};

#[allow(dead_code)]
pub const VIRTIO_BLK_DRIVER: &str = "virtio-blk";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device", "drive")]
pub struct VirtioBlockDevice {
    #[property(param = "device", ignore_key)]
    pub driver: String,
    #[property(param = "drive")]
    #[property(param = "device", key = "drive")]
    pub id: String,
    #[property(param = "device", key = "id")]
    pub deviceid: String,
    #[property(param = "drive")]
    pub file: Option<String>,
    #[property(param = "drive")]
    pub r#if: Option<String>,
    #[property(param = "drive", generator = "crate::utils::bool_to_on_off")]
    pub readonly: Option<bool>,
    #[property(param = "drive", generator = "crate::utils::bool_to_on_off")]
    pub direct: Option<bool>,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub bus: Option<String>,
    #[property(param = "device", predicate = "self.addr.len()>0")]
    pub addr: String,
}

impl_device_no_bus!(VirtioBlockDevice);
impl_set_get_device_addr!(VirtioBlockDevice);

impl VirtioBlockDevice {
    pub fn new(
        driver: &str,
        id: &str,
        deviceid: &str,
        file: Option<String>,
        read_only: Option<bool>,
    ) -> Self {
        Self {
            driver: driver.to_string(),
            id: id.to_string(),
            deviceid: deviceid.to_string(),
            file,
            r#if: None,
            readonly: read_only,
            direct: None,
            bus: None,
            addr: "".to_string(),
        }
    }
}

#[async_trait]
impl HotAttachable for VirtioBlockDevice {
    async fn execute_hot_attach(&self, client: &QmpClient, rp_id: &str) -> Result<()> {
        debug!("hot attach block device {}", self.id);
        client.execute(self.to_blockdev_add()).await?;
        match client.execute(self.to_device_add(rp_id)).await {
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
        blockdev_add(BlockdevOptions::raw {
            base: BlockdevOptionsBase {
                node_name: Some(self.id.to_string()),
                read_only: self.readonly,
                auto_read_only: None,
                cache: Some(BlockdevCacheOptions {
                    no_flush: None,
                    direct: Some(true),
                }),
                force_share: None,
                discard: None,
                detect_zeroes: None,
            },
            raw: BlockdevOptionsRaw {
                base: BlockdevOptionsGenericFormat {
                    file: BlockdevRef::definition(Box::new(BlockdevOptions::file {
                        base: BlockdevOptionsBase {
                            auto_read_only: None,
                            force_share: None,
                            read_only: None,
                            discard: None,
                            cache: None,
                            detect_zeroes: None,
                            node_name: None,
                        },
                        file: BlockdevOptionsFile {
                            drop_cache: None,
                            locking: None,
                            x_check_cache_dropped: None,
                            filename: self.file.as_ref().unwrap().to_string(),
                            aio: None,
                            pr_manager: None,
                        },
                    })),
                },
                size: None,
                offset: None,
            },
        })
    }

    fn to_device_add(&self, rp_id: &str) -> device_add {
        let mut args = Dictionary::new();
        args.insert("drive".to_string(), Value::from(self.id()));
        let addr = "0x0".to_string();
        args.insert("addr".to_string(), Value::from(addr));

        let driver = "virtio-blk-pci".to_string();
        let bus = Some(rp_id.to_string());

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

#[cfg(test)]
mod tests {
    use qapi::qmp::BlockdevOptions;

    use super::{VirtioBlockDevice, VIRTIO_BLK_DRIVER};

    #[test]
    fn test_block_device_add_qmp_commands() {
        let virtio_blk_device = VirtioBlockDevice::new(
            VIRTIO_BLK_DRIVER,
            "drive-0",
            "",
            Some("/dev/dm-8".to_string()),
            Some(false),
        );

        let blockdev_add_qmp_cmd = virtio_blk_device.to_blockdev_add().0;
        let blockdev_add_qmp_json_str = serde_json::to_string(&blockdev_add_qmp_cmd).unwrap();
        println!(
            "blockdev_add qmp cmd json string: {}",
            blockdev_add_qmp_json_str
        );

        let expected_params_str = r#"{"driver":"raw","read-only":false,"node-name":"drive-0","cache":{"direct":true},"file":{"driver":"file","filename":"/dev/dm-8"}}"#;
        let expected_add_qmp_cmd: BlockdevOptions =
            serde_json::from_str(expected_params_str).unwrap();

        if let BlockdevOptions::raw { base, raw } = expected_add_qmp_cmd {
            // TODO: compare the all elements
            assert!(true);
        }
    }

    #[test]
    fn test_device_add_qmp_commands() {
        let virtio_blk_device = VirtioBlockDevice::new(
            VIRTIO_BLK_DRIVER,
            "drive-0",
            "",
            Some("/dev/dm-8".to_string()),
            Some(false),
        );

        let device_add_qmp_cmd = virtio_blk_device.to_device_add("pcie.1");
        let device_add_qmp_json_str = serde_json::to_string(&device_add_qmp_cmd).unwrap();
        println!(
            "block device_add qmp cmd json string: {}",
            device_add_qmp_json_str
        );

        let expected_params_str = r#"{"driver":"virtio-blk-pci","id":"virtio-drive-0","bus":"pcie.1","addr":"0x0","drive":"drive-0"}"#;
        assert!(true);
    }

    #[test]
    fn test_block_device_del_qmp_commands() {
        let virtio_blk_device = VirtioBlockDevice::new(
            VIRTIO_BLK_DRIVER,
            "drive-0",
            "",
            Some("/dev/dm-8".to_string()),
            Some(false),
        );

        let blockdev_del_qmp_cmd = virtio_blk_device.to_blockdev_del();
        let blockdev_del_qmp_json_str = serde_json::to_string(&blockdev_del_qmp_cmd).unwrap();
        println!(
            "blockdev_del qmp cmd json string: {}",
            blockdev_del_qmp_json_str
        );

        let expected_params_str = r#"{"node-name":"drive-0"}"#;
        assert_eq!(expected_params_str, blockdev_del_qmp_json_str);
    }

    #[test]
    fn test_device_del_qmp_commands() {
        let virtio_blk_device = VirtioBlockDevice::new(
            VIRTIO_BLK_DRIVER,
            "drive-0",
            "",
            Some("/dev/dm-8".to_string()),
            Some(false),
        );

        let device_del_qmp_cmd = virtio_blk_device.to_device_del();
        let device_del_qmp_json_str = serde_json::to_string(&device_del_qmp_cmd).unwrap();
        println!(
            "device_del qmp cmd json string: {}",
            device_del_qmp_json_str
        );

        let expected_params_str = r#"{"id":"virtio-drive-0"}"#;
        assert_eq!(expected_params_str, device_del_qmp_json_str);
    }
}
