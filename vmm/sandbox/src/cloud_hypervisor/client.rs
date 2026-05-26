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
    os::unix::{
        io::{AsRawFd, RawFd},
        net::UnixStream,
    },
    thread::sleep,
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use api_client::{simple_api_command, simple_api_full_command_with_fds_and_response};
use containerd_sandbox::error::Result;
use log::{debug, error, trace};
use serde_json::json;
use tokio::task::spawn_blocking;

use crate::{
    cloud_hypervisor::devices::{block::DiskConfig, AddDeviceResponse, RemoveDeviceRequest},
    device::DeviceInfo,
    sandbox::MemoryRestoreMode,
};

pub(crate) const CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC: u64 = 10;

pub struct ChClient {
    socket: UnixStream,
}

impl ChClient {
    pub async fn new(socket_path: String) -> Result<Self> {
        let s = socket_path.to_string();
        let start_time = SystemTime::now();
        let socket = spawn_blocking(move || -> Result<UnixStream> {
            loop {
                match UnixStream::connect(&socket_path) {
                    Ok(socket) => {
                        return Ok(socket);
                    }
                    Err(e) => {
                        trace!("failed to create client: {:?}", e);
                        if start_time.elapsed().unwrap().as_secs()
                            > CLOUD_HYPERVISOR_START_TIMEOUT_IN_SEC
                        {
                            error!("failed to create client: {:?}", e);
                            return Err(anyhow!("timeout connect client, {}", e).into());
                        }
                        sleep(Duration::from_millis(10));
                    }
                }
            }
        })
        .await
        .map_err(|e| anyhow!("failed to join thread {}", e))??;
        debug!("connected to api server {}", s);
        Ok(Self { socket })
    }

    pub fn hot_attach(&mut self, device_info: DeviceInfo, direct_io: bool) -> Result<String> {
        match device_info {
            DeviceInfo::Block(blk) => {
                let disk_config = DiskConfig {
                    path: blk.path,
                    readonly: blk.read_only,
                    direct: direct_io,
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
            DeviceInfo::Tap(tap) => {
                if tap.fds.is_empty() {
                    return Err(anyhow!("tap device '{}': fds must not be empty", tap.id).into());
                }
                let raw_fds: Vec<RawFd> = tap.fds.iter().map(|fd| fd.as_raw_fd()).collect();
                let num_queues = (raw_fds.len() as u32) * 2;
                self.vm_add_net(&tap.id, &tap.mac_address, num_queues, raw_fds)?;
                // net hotplug succeeds without a BDF response from CH
                Ok(String::new())
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

    pub fn vm_pause(&mut self) -> Result<()> {
        tokio::task::block_in_place(|| {
            simple_api_command(&mut self.socket, "PUT", "pause", None)
                .map_err(|e| anyhow!("vm.pause: {}", e).into())
        })
    }

    pub fn vm_resume(&mut self) -> Result<()> {
        tokio::task::block_in_place(|| {
            simple_api_command(&mut self.socket, "PUT", "resume", None)
                .map_err(|e| anyhow!("vm.resume: {}", e).into())
        })
    }

    pub fn vm_snapshot(&mut self, dest_url: &str) -> Result<()> {
        tokio::task::block_in_place(|| {
            let body = json!({ "destination_url": dest_url }).to_string();
            simple_api_command(&mut self.socket, "PUT", "snapshot", Some(&body))
                .map_err(|e| anyhow!("vm.snapshot: {}", e).into())
        })
    }

    pub fn vm_restore(
        &mut self,
        source_url: &str,
        memory_restore_mode: &MemoryRestoreMode,
    ) -> Result<()> {
        tokio::task::block_in_place(|| {
            let body =
                json!({ "source_url": source_url, "memory_restore_mode": memory_restore_mode })
                    .to_string();
            simple_api_command(&mut self.socket, "PUT", "restore", Some(&body))
                .map_err(|e| anyhow!("vm.restore: {}", e).into())
        })
    }

    /// Hotplug a network interface into the running VM.
    ///
    /// `fds` are tap queue file descriptors sent to CH via SCM_RIGHTS ancillary data.
    /// `num_queues` must equal `fds.len() * 2` (one rx + one tx queue per fd).
    /// The FDs must remain open until this call returns (CH receives its copies via SCM_RIGHTS).
    pub fn vm_add_net(
        &mut self,
        id: &str,
        mac: &str,
        num_queues: u32,
        fds: Vec<RawFd>,
    ) -> Result<()> {
        tokio::task::block_in_place(|| {
            let body = json!({
                "id": id,
                "mac": mac,
                "num_queues": num_queues,
                "queue_size": 256,
                "vhost_user": false,
            })
            .to_string();
            simple_api_full_command_with_fds_and_response(
                &mut self.socket,
                "PUT",
                "vm.add-net",
                Some(&body),
                fds,
            )
            .map_err(|e| anyhow!("vm.add-net {}: {}", id, e))?;
            Ok(())
        })
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
