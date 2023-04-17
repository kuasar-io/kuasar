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
use containerd_sandbox::error::Result;
use log::{debug, error};
use qapi::{
    qmp::{
        chardev_add, chardev_remove, device_add, device_del, ChardevBackend, ChardevCommon,
        ChardevHostdev,
    },
    Dictionary,
};
use sandbox_derive::CmdLineParams;
use serde_json::Value;

use crate::{
    device::{BusType, CharBackendType},
    qemu::{devices::HotAttachable, qmp_client::QmpClient},
};

pub const VIRT_CONSOLE_DRIVER: &str = "virtconsole";
pub const VIRT_SERIAL_PORT_DRIVER: &str = "virtserialport";

pub const CHARBACKEND_SOCKET: &str = "socket";
pub const CHARBACKEND_PIPE: &str = "pipe";

#[derive(CmdLineParams, Debug, Clone)]
#[params("device", "chardev")]
pub struct CharDevice {
    #[property(param = "chardev", ignore_key)]
    pub backend: String,
    #[property(param = "device", ignore_key)]
    pub driver: String,
    #[property(param = "device", key = "bus")]
    pub bus_addr: Option<String>,
    #[property(param = "device")]
    pub id: String,
    #[property(param = "chardev", key = "id")]
    #[property(param = "device", key = "chardev")]
    pub chardev_id: String,
    #[property(param = "chardev")]
    pub path: String,
    #[property(param = "device")]
    pub name: Option<String>,
    #[property(param = "device")]
    pub disable_modern: Option<bool>,
    #[property(param = "device")]
    pub romfile: Option<String>,
    #[property(
        param = "chardev",
        predicate = "self.backend == \"socket\"",
        generator = "crate::utils::bool_to_on_off"
    )]
    pub server: bool,
    #[property(
        param = "chardev",
        predicate = "self.backend == \"socket\"",
        generator = "crate::utils::bool_to_on_off"
    )]
    pub wait: bool,
}

impl_device_no_bus!(CharDevice);

impl CharDevice {
    pub fn new_with_backend_type(
        backend_type: CharBackendType,
        id: &str,
        chardev_id: &str,
        driver: &str,
        name: Option<String>,
    ) -> Self {
        match backend_type {
            CharBackendType::Pipe(p) => CharDevice::new_pipe(id, chardev_id, &p, driver, name),
            CharBackendType::Socket(p) => CharDevice::new_socket(id, chardev_id, &p, driver, name),
        }
    }
    pub fn new_socket(
        id: &str,
        chardev_id: &str,
        path: &str,
        driver: &str,
        name: Option<String>,
    ) -> Self {
        Self::new(CHARBACKEND_SOCKET, id, chardev_id, path, driver, name)
    }

    pub fn new_pipe(
        id: &str,
        chardev_id: &str,
        path: &str,
        driver: &str,
        name: Option<String>,
    ) -> Self {
        Self::new(CHARBACKEND_PIPE, id, chardev_id, path, driver, name)
    }

    fn new(
        backend: &str,
        id: &str,
        chardev_id: &str,
        path: &str,
        driver: &str,
        name: Option<String>,
    ) -> Self {
        Self {
            backend: backend.to_string(),
            driver: driver.to_string(),
            bus_addr: None,
            id: id.to_string(),
            chardev_id: chardev_id.to_string(),
            path: path.to_string(),
            name,
            disable_modern: None,
            romfile: None,
            server: true,
            wait: false,
        }
    }
}

#[async_trait]
impl HotAttachable for CharDevice {
    async fn execute_hot_attach(
        &self,
        client: &QmpClient,
        _bus_type: &BusType,
        _bus_id: &str,
        _slot_index: usize,
    ) -> Result<()> {
        debug!("execute hot attach {:?}", self);
        client.execute(self.to_chardev_add()?).await?;
        match client.execute(self.to_device_add()).await {
            Ok(_) => Ok(()),
            Err(e) => {
                client
                    .execute(self.to_chardev_remove())
                    .await
                    .unwrap_or_else(|e| {
                        error!("failed to delete blockdev after device_add failed, {}", e);
                        qapi::Empty {}
                    });
                Err(e)
            }
        }
    }

    async fn execute_hot_detach(&self, client: &QmpClient) -> Result<()> {
        debug!("hot detach {:?}", self);
        client.delete_device(&self.id).await?;
        client.execute(self.to_chardev_remove()).await?;
        Ok(())
    }
}

impl CharDevice {
    fn to_chardev_add(&self) -> Result<chardev_add> {
        match &*self.backend {
            CHARBACKEND_PIPE => Ok(chardev_add {
                id: self.chardev_id.to_string(),
                backend: ChardevBackend::pipe {
                    data: ChardevHostdev {
                        base: ChardevCommon {
                            logappend: None,
                            logfile: None,
                        },
                        device: self.path.to_string(),
                    },
                },
            }),
            _ => Err(anyhow!("no support hotplug of char device other than pipe").into()),
        }
    }

    fn to_device_add(&self) -> device_add {
        let mut args = Dictionary::new();
        if let Some(x) = self.name.as_ref() {
            args.insert("name".to_string(), Value::from(x.to_string()));
        }
        args.insert(
            "chardev".to_string(),
            Value::from(self.chardev_id.to_string()),
        );
        device_add {
            driver: self.driver.to_string(),
            bus: None,
            id: Some(self.id.to_string()),
            arguments: args,
        }
    }

    fn to_chardev_remove(&self) -> chardev_remove {
        chardev_remove {
            id: self.chardev_id.to_string(),
        }
    }

    #[allow(dead_code)]
    fn to_device_del(&self) -> device_del {
        device_del {
            id: self.id.to_string(),
        }
    }
}
