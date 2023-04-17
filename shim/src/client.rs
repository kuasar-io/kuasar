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
    os::unix::io::{IntoRawFd, RawFd},
    time::Duration,
};

use containerd_shim::{
    error::{Error, Result},
    other, other_error,
    protos::shim_async::{Client, TaskClient},
};
use tokio::{net::UnixStream, time::timeout};

pub const HVSOCK_PREFIX: &str = "hvsock://";

pub(crate) fn uds_task_client(address: &str) -> Result<TaskClient> {
    let client = Client::connect(address)?;
    Ok(TaskClient::new(client))
}

// Client to connect vm hvsock, used to call task service
pub(crate) async fn vsock_task_client(address: &str) -> Result<TaskClient> {
    let (addr, port) = match address.strip_prefix(HVSOCK_PREFIX) {
        None => {
            return Err(other!("task address {} should have prefix hvsock", address));
        }
        Some(address) => {
            let v: Vec<&str> = address.split(':').collect();
            if v.len() < 2 {
                return Err(other!("hvsock address {} should not less than 2", address));
            }
            (v[0], v[1])
        }
    };

    let ctx_timeout = 2;
    let mut last_err = other!("");

    let fut = async {
        loop {
            match connect_to_hvsocket(addr, port).await {
                Ok(fd) => {
                    let client = Client::new(fd);
                    return client;
                }
                Err(e) => last_err = e,
            }
        }
    };

    let client = timeout(Duration::from_secs(ctx_timeout), fut)
        .await
        .map_err(|_| other!("{}s timeout connecting socket: {}", ctx_timeout, last_err))?;
    Ok(TaskClient::new(client))
}

async fn connect_to_hvsocket(addr: &str, port: &str) -> Result<RawFd> {
    loop {
        let stream = UnixStream::connect(addr)
            .await
            .map_err(other_error!(e, "can't connect"))?;
        stream
            .writable()
            .await
            .map_err(other_error!(e, "not writable"))?;
        match stream.try_write(format!("CONNECT {port}\n").as_bytes()) {
            Ok(_) => {
                stream
                    .readable()
                    .await
                    .map_err(other_error!(e, "not readable"))?;
                let mut buf = [0; 4096];
                match stream.try_read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        if String::from_utf8(buf[..n].to_vec())
                            .unwrap_or_default()
                            .contains("OK")
                        {
                            return Ok(stream
                                .into_std()
                                .map_err(other_error!(e, "not std stream"))?
                                .into_raw_fd());
                        }
                        continue;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(e) => {
                        return Err(other!("failed to read from hvsock: {}", e));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(e) => {
                return Err(other!("failed to write CONNECT to hvsock: {}", e));
            }
        }
    }
}
