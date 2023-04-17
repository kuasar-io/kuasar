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

use containerd_shim::{error::Error, io_error, other};
use tokio_vsock::VsockListener;

pub async fn bind_vsock(addr: &str) -> containerd_shim::Result<VsockListener> {
    let addr = addr.strip_prefix("vsock://").unwrap_or(addr);
    let addr = addr.strip_prefix("hvsock://").unwrap_or(addr);
    let cid_port: Vec<&str> = addr.split(':').collect();
    if cid_port.len() != 2 {
        return Err(other!("{} is not a valid vsock addr", addr));
    }
    let port: u32 = cid_port[1].parse().map_err(Error::ParseInt)?;
    let l = VsockListener::bind(0xFFFFFFFF, port).map_err(io_error!(
        e,
        "failed to listen vsock port {}",
        port
    ))?;
    Ok(l)
}
