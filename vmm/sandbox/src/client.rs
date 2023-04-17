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

use anyhow::anyhow;
use containerd_sandbox::error::{Error, Result};
use log::error;
use nix::{
    sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr, VsockAddr},
    unistd::close,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};
use ttrpc::{context::with_timeout, r#async::Client};
use vmm_common::api::{sandbox::*, sandbox_ttrpc::SandboxServiceClient};

use crate::network::{NetworkInterface, Route};

const HVSOCK_RETRY_TIMEOUT: u64 = 10;

pub(crate) async fn new_sandbox_client(address: &str) -> Result<SandboxServiceClient> {
    let client = new_ttrpc_client(address).await?;
    Ok(SandboxServiceClient::new(client))
}

async fn new_ttrpc_client(address: &str) -> Result<Client> {
    let ctx_timeout = 10;

    let mut last_err = Error::Other(anyhow!(""));

    let fut = async {
        loop {
            match connect_to_socket(address).await {
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
        .map_err(|_| anyhow!("{}s timeout connecting socket: {}", ctx_timeout, last_err))?;
    Ok(client)
}

// Supported sock address formats are:
//   - unix://<unix socket path>
//   - vsock://<cid>:<port>
//   - <unix socket path>
//   - hvsock://<unix socket path>:<port>, eg: hvsock:///var/lib/kuasar/75e168af2da4c40fa0fc45a0480be18e2c92e33e6f7e2756cf8d92c268e7370d/task.vsock:1024
pub async fn connect_to_socket(address: &str) -> Result<RawFd> {
    if let Some(addr) = address.strip_prefix("unix://") {
        return connect_to_unix_socket(addr).await;
    }

    if let Some(addr) = address.strip_prefix("vsock://") {
        return connect_to_vsocket(addr).await;
    }

    if let Some(addr) = address.strip_prefix("hvsock://") {
        return connect_to_hvsocket(addr).await;
    }

    connect_to_unix_socket(address).await
}

async fn connect_to_unix_socket(address: &str) -> Result<RawFd> {
    let sockaddr = unix_sock(false, address)?;

    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("failed to create socket fd: {}", e))?;

    tokio::task::spawn_blocking(move || {
        connect(fd, &sockaddr).map_err(|e| {
            close(fd).unwrap_or_else(|ce| {
                error!("failed to close fd: {}", ce);
            });
            anyhow!("failed to connect {} :{}", sockaddr, e)
        })
    })
    .await
    .map_err(|e| anyhow!("failed to spawn blocking task: {}", e))??;

    Ok(fd)
}

async fn connect_to_vsocket(address: &str) -> Result<RawFd> {
    let (cid, port) = {
        let v: Vec<String> = address.split(':').map(String::from).collect();
        if v.len() < 2 {
            return Err(anyhow!("vsock address {} should not less than 2", address).into());
        }
        let cid = v[0]
            .parse()
            .map_err(|e| anyhow!("failed to parse vsock cid {}: {}", address, e))?;
        let port = v[1]
            .parse()
            .map_err(|e| anyhow!("failed to parse vsock port {}: {}", address, e))?;
        (cid, port)
    };

    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("failed to create vsocket fd: {}", e))?;

    let sockaddr = VsockAddr::new(cid, port);
    tokio::task::spawn_blocking(move || {
        connect(fd, &sockaddr).map_err(|e| {
            close(fd).unwrap_or_else(|ce| {
                error!("failed to close fd: {}", ce);
            });
            anyhow!("failed to connect {} :{}", sockaddr, e)
        })
    })
    .await
    .map_err(|e| anyhow!("failed to spawn blocking task: {}", e))??;

    Ok(fd)
}

async fn connect_to_hvsocket(address: &str) -> Result<RawFd> {
    let (addr, port) = {
        let v: Vec<&str> = address.split(':').collect();
        if v.len() < 2 {
            return Err(anyhow!("hvsock address {} should not less than 2", address).into());
        }
        (v[0], v[1])
    };

    let fut = async {
        let mut stream = UnixStream::connect(addr).await?;

        match stream.write(format!("CONNECT {}\n", port).as_bytes()).await {
            Ok(_) => {
                let mut buf = [0; 4096];
                match stream.read(&mut buf).await {
                    Ok(0) => Err(anyhow!("stream closed")),
                    Ok(n) => {
                        if String::from_utf8(buf[..n].to_vec())
                            .unwrap_or_default()
                            .contains("OK")
                        {
                            return Ok(stream.into_std()?.into_raw_fd());
                        }
                        Err(anyhow!("failed to connect"))
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        Err(anyhow!("{}", e))
                    }
                    Err(e) => Err(anyhow!("failed to read from hvsock: {}", e)),
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(anyhow!("{}", e)),
            Err(e) => Err(anyhow!("failed to write CONNECT to hvsock: {}", e)),
        }
        .map_err(Error::Other)
    };

    timeout(Duration::from_millis(HVSOCK_RETRY_TIMEOUT), fut)
        .await
        .map_err(|_| anyhow!("hvsock retry {}ms timeout", HVSOCK_RETRY_TIMEOUT))?
}

pub fn unix_sock(r#abstract: bool, socket_path: &str) -> Result<UnixAddr> {
    let sockaddr_u = if r#abstract {
        let sockaddr_h = socket_path.to_owned() + "\x00";
        UnixAddr::new_abstract(sockaddr_h.as_bytes())
    } else {
        UnixAddr::new(socket_path)
    }
    .map_err(|e| anyhow!("failed to new socket: {}", e))?;
    Ok(sockaddr_u)
}

pub(crate) async fn client_check(client: &SandboxServiceClient) -> Result<()> {
    // the initial timeout is 1, and will grow exponentially
    let retry_timeout = 1;
    let ctx_timeout = 45;

    let res_fut = do_check_agent(client, retry_timeout);
    timeout(Duration::from_secs(ctx_timeout), res_fut)
        .await
        .map_err(|_| anyhow!("{}s timeout checking", ctx_timeout))?;
    Ok(())
}

async fn do_check_agent(client: &SandboxServiceClient, timeout: u64) {
    let req = CheckRequest::new();
    let duration = Duration::from_secs(timeout).as_nanos() as i64;
    loop {
        if client.check(with_timeout(duration), &req).await.is_ok() {
            return;
        };
    }
}

pub(crate) async fn client_update_interfaces(
    client: &SandboxServiceClient,
    intfs: &[NetworkInterface],
) -> Result<()> {
    let mut req = UpdateInterfacesRequest::new();
    req.interfaces = intfs.iter().map(|x| x.into()).collect();

    client
        .update_interfaces(
            with_timeout(Duration::from_secs(10).as_nanos() as i64),
            &req,
        )
        .await
        .map_err(|e| anyhow!("failed to update interfaces: {}", e))?;
    Ok(())
}

pub(crate) async fn client_update_routes(
    client: &SandboxServiceClient,
    rts: &[Route],
) -> Result<()> {
    let mut req = UpdateRoutesRequest::new();
    req.routes = rts.iter().map(|x| x.into()).collect();

    client
        .update_routes(with_timeout(Duration::from_secs(3).as_nanos() as i64), &req)
        .await
        .map_err(|e| anyhow!("failed to update routes: {}", e))?;
    Ok(())
}
