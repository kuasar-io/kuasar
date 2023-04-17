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

use std::os::unix::io::{IntoRawFd, RawFd};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use nix::ioctl_write_ptr_bad;
use rand::{thread_rng, Rng};
use sandbox_derive::CmdLineParams;

use crate::device::Transport;

const VHOST_VSOCK_DEV_PATH: &str = "/dev/vhost-vsock";
const IOCTL_VHOST_SET_GUEST_ID: u64 = 0x4008AF60;
const IOCTL_TRY_TIMES: u64 = 10000;
const VHOST_VSOCK_DRIVER: &str = "vhost-vsock";

ioctl_write_ptr_bad!(set_vhost_guest_cid, IOCTL_VHOST_SET_GUEST_ID, u64);

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct VSockDevice {
    #[property(ignore_key)]
    pub driver: String,
    pub id: String,
    #[property(key = "guest-cid")]
    pub context_id: u64,
    #[property(key = "vhostfd")]
    pub vhost_fd_index: Option<i32>,
    pub romfile: Option<String>,
    pub disable_modern: Option<bool>,
}

impl_device_no_bus!(VSockDevice);

impl VSockDevice {
    pub fn new(context_id: u64, fd_index: Option<i32>, transport: Transport) -> Self {
        Self {
            driver: transport.to_driver(VHOST_VSOCK_DRIVER),
            id: format!("vsock-{}", context_id),
            context_id,
            vhost_fd_index: fd_index,
            romfile: None,
            disable_modern: None,
        }
    }
}

pub async fn find_context_id() -> Result<(RawFd, u64)> {
    // TODO make sure if this thread_rng is enough, if we should new a seedable rng everytime.
    let vsock_file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .mode(0o666)
        .open(VHOST_VSOCK_DEV_PATH)
        .await?;
    let vsockfd = vsock_file.into_std().await.into_raw_fd();
    for _i in 0..IOCTL_TRY_TIMES {
        let cid = thread_rng().gen_range(3..i32::MAX as u64);
        let res = unsafe { set_vhost_guest_cid(vsockfd, &cid) };
        match res {
            Ok(_) => return Ok((vsockfd, cid)),
            Err(_) => continue,
        }
    }
    Err(anyhow!("tried 10000 times, but can not set guest cid".to_string()).into())
}
