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
    os::unix::net::UnixStream,
    thread::sleep,
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use api_client::{simple_api_command, simple_api_full_command_with_fds_and_response};
use containerd_sandbox::error::Result;
use log::error;

use crate::{
    cloud_hypervisor::devices::{block::DiskConfig, AddDeviceResponse, RemoveDeviceRequest},
    device::DeviceInfo,
};

pub(crate) const CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC: u64 = 10;

pub struct ChClient {
    socket: UnixStream,
}

impl ChClient {
    pub async fn new(socket_path: String) -> Result<Self> {
        let start_time = SystemTime::now();
        tokio::task::spawn_blocking(move || loop {
            match UnixStream::connect(&socket_path) {
                Ok(socket) => {
                    return Ok(Self { socket });
                }
                Err(e) => {
                    if start_time.elapsed().unwrap().as_secs()
                        > CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC
                    {
                        error!("failed to connect api server: {:?}", e);
                        return Err(anyhow!("timeout connect client, {}", e).into());
                    }
                    sleep(Duration::from_millis(10));
                }
            }
        })
        .await
        .map_err(|e| anyhow!("failed to spawn a task {}", e))?
    }

    pub fn hot_attach(&mut self, device_info: DeviceInfo) -> Result<String> {
        match device_info {
            DeviceInfo::Block(blk) => {
                let disk_config = DiskConfig {
                    path: blk.path,
                    readonly: blk.read_only,
                    direct: true,
                    vhost_user: false,
                    vhost_socket: None,
                    id: blk.id,
                };
                let request_body = serde_json::to_string(&disk_config)
                    .map_err(|e| anyhow!("failed to marshal {:?} to json, {}", disk_config, e))?;
                let response_opt = simple_api_full_command_with_fds_and_response(
                    &mut self.socket,
                    "PUT",
                    "vm.add-disk",
                    Some(&request_body),
                    vec![],
                )
                .map_err(|e| anyhow!("failed to hotplug disk {}, {}", request_body, e))?;
                if let Some(response_body) = response_opt {
                    let response = serde_json::from_str::<AddDeviceResponse>(&response_body)
                        .map_err(|e| {
                            anyhow!("failed to unmarshal response {}, {}", response_body, e)
                        })?;
                    Ok(response.bdf)
                } else {
                    Err(anyhow!("no response body from server").into())
                }
            }
            DeviceInfo::Tap(_) => {
                todo!()
            }
            DeviceInfo::Physical(_) => {
                todo!()
            }
            DeviceInfo::VhostUser(_) => {
                todo!()
            }
            DeviceInfo::Char(_) => {
                unimplemented!()
            }
        }
    }

    pub fn hot_detach(&mut self, device_id: &str) -> Result<()> {
        let request = RemoveDeviceRequest {
            id: device_id.to_string(),
        };
        let request_body = serde_json::to_string(&request)
            .map_err(|e| anyhow!("failed to marshal {:?} to json, {}", request, e))?;
        simple_api_command(
            &mut self.socket,
            "PUT",
            "remove-device",
            Some(&request_body),
        )
        .map_err(|e| anyhow!("failed to remove device {}, {}", request_body, e))?;
        Ok(())
    }
}
