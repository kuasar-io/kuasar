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

use std::{collections::HashSet, str::FromStr};

use async_trait::async_trait;
use containerd_shim::{
    error::{Error, Result},
    io::Stdio as ShimStdio,
    io_error, other, other_error,
    protos::shim_async::TaskClient,
};
use lazy_static::lazy_static;
use log::{error, warn};
use tokio::{
    fs::OpenOptions,
    io::{AsyncRead, AsyncWrite},
    net::UnixStream,
    sync::Mutex,
};

use crate::{
    client::{uds_task_client, vsock_task_client, HVSOCK_PREFIX},
    sandbox::{VMM_SANDBOXER_SOCKET_PATH, WASM_SANDBOXER_SOCKET_PATH},
};

lazy_static! {
    static ref PORTMAP: Mutex<HashSet<i32>> = Mutex::new(HashSet::new());
}

#[derive(Debug, Clone)]
pub struct UdsTransport {
    pub containerd_pipe: ShimStdio,
}

#[derive(Debug, Clone)]
pub struct VSockTransport {
    pub stdin: Option<VsockIO>,
    pub stdout: Option<VsockIO>,
    pub stderr: Option<VsockIO>,

    pub containerd_pipe: ShimStdio,
}

// TODO: add Default trait bound for ShimStdio
impl Default for UdsTransport {
    fn default() -> Self {
        Self {
            containerd_pipe: ShimStdio {
                stdin: Default::default(),
                stdout: Default::default(),
                stderr: Default::default(),
                terminal: Default::default(),
            },
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct VsockIO {
    sock_path: String,
    port: i32,
}

impl VsockIO {
    pub fn new(path: &str, port: i32) -> Self {
        Self {
            sock_path: path.to_string(),
            port,
        }
    }

    pub async fn connect(&self) -> Result<UnixStream> {
        for _i in 0..100 {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            let stream = UnixStream::connect(self.sock_path.as_str())
                .await
                .map_err(other_error!(e, "can't connect"))?;
            stream
                .writable()
                .await
                .map_err(other_error!(e, "not writable"))?;
            match stream.try_write(format!("CONNECT {}\n", self.port).as_bytes()) {
                Ok(_) => {
                    stream
                        .readable()
                        .await
                        .map_err(other_error!(e, "not readable"))?;
                    let mut buf = [0; 16];
                    match stream.try_read(&mut buf) {
                        Ok(0) => continue,
                        Ok(n) => {
                            if String::from_utf8(buf[..n].to_vec())
                                .unwrap_or_default()
                                .contains("OK")
                            {
                                return Ok(stream);
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
        Err(other!("timeout connect to port {}", self.port))
    }
}

impl ToString for VsockIO {
    fn to_string(&self) -> String {
        if self.sock_path.is_empty() {
            String::new()
        } else {
            format!("{}{}:{}", HVSOCK_PREFIX, self.sock_path, self.port)
        }
    }
}

#[async_trait]
pub trait ContainerIoTransport: Sync + Send + Default + Clone {
    async fn new(
        task_addr: String,
        in_pipe: String,
        out_pipe: String,
        err_pipe: String,
        terminal: bool,
    ) -> Result<Self>;

    fn sandboxer_addr() -> String;

    async fn copy(&self) -> Result<()>;

    fn container_in(&self) -> String;

    fn container_out(&self) -> String;

    fn container_err(&self) -> String;

    async fn new_task_client(address: &str) -> Result<TaskClient>;

    async fn cleanup_connection(self);
}

// TODO: add Default trait bound for ShimStdio
impl Default for VSockTransport {
    fn default() -> Self {
        Self {
            stdin: Default::default(),
            stdout: Default::default(),
            stderr: Default::default(),
            containerd_pipe: ShimStdio {
                stdin: Default::default(),
                stdout: Default::default(),
                stderr: Default::default(),
                terminal: Default::default(),
            },
        }
    }
}

#[async_trait]
impl ContainerIoTransport for UdsTransport {
    async fn new(
        _task_addr: String,
        in_pipe: String,
        out_pipe: String,
        err_pipe: String,
        terminal: bool,
    ) -> Result<Self> {
        Ok(Self {
            containerd_pipe: ShimStdio {
                stdin: in_pipe,
                stdout: out_pipe,
                stderr: err_pipe,
                terminal,
            },
        })
    }

    fn sandboxer_addr() -> String {
        WASM_SANDBOXER_SOCKET_PATH.to_string()
    }

    async fn copy(&self) -> Result<()> {
        Ok(())
    }

    fn container_in(&self) -> String {
        self.containerd_pipe.stdin.clone()
    }

    fn container_out(&self) -> String {
        self.containerd_pipe.stdout.clone()
    }

    fn container_err(&self) -> String {
        self.containerd_pipe.stderr.clone()
    }

    async fn new_task_client(address: &str) -> Result<TaskClient> {
        uds_task_client(address)
    }

    async fn cleanup_connection(self) {}
}
#[async_trait]
impl ContainerIoTransport for VSockTransport {
    async fn new(
        task_addr: String,
        in_pipe: String,
        out_pipe: String,
        err_pipe: String,
        terminal: bool,
    ) -> Result<Self> {
        let sock_path = {
            let v = task_addr
                .trim_start_matches(HVSOCK_PREFIX)
                .split_once(':')
                .ok_or(other!("wrong hvsock address {}", task_addr))?;
            v.0
        };

        let mut std_i = None;
        let mut std_o = None;
        let mut std_e = None;

        if !in_pipe.is_empty() {
            std_i = Some(VsockIO::new(sock_path, Self::find_available_port().await?));
        }

        if !out_pipe.is_empty() {
            std_o = Some(VsockIO::new(sock_path, Self::find_available_port().await?));
        }

        if !err_pipe.is_empty() && !terminal {
            std_e = Some(VsockIO::new(sock_path, Self::find_available_port().await?));
        }

        Ok(Self {
            stdin: std_i,
            stdout: std_o,
            stderr: std_e,
            containerd_pipe: ShimStdio {
                stdin: in_pipe,
                stdout: out_pipe,
                stderr: err_pipe,
                terminal,
            },
        })
    }

    fn sandboxer_addr() -> String {
        VMM_SANDBOXER_SOCKET_PATH.to_string()
    }

    async fn copy(&self) -> Result<()> {
        if let Some(stdin) = self.stdin.as_ref() {
            let stream = stdin.connect().await?;
            let c_stdin = OpenOptions::new()
                .read(true)
                .open(self.containerd_pipe.stdin.as_str())
                .await
                .map_err(io_error!(e, "open stdin"))?;
            spawn_copy(c_stdin, stream, None::<fn()>);
        }

        if let Some(stdout) = self.stdout.as_ref() {
            let stream = stdout.connect().await?;
            let c_stdout = OpenOptions::new()
                .write(true)
                .open(self.containerd_pipe.stdout.as_str())
                .await
                .map_err(io_error!(e, "open stdout for read"))?;
            // open a read to make sure even if the read end of containerd shutdown,
            // copy still continue until the restart of containerd succeed
            let c_stdout_r = OpenOptions::new()
                .read(true)
                .open(self.containerd_pipe.stdout.as_str())
                .await
                .map_err(io_error!(e, "open stdout for read"))?;
            spawn_copy(
                stream,
                c_stdout,
                Some(move || {
                    drop(c_stdout_r);
                }),
            );
        }

        if let Some(stderr) = self.stderr.as_ref() {
            let stream = stderr.connect().await?;
            let c_stderr = OpenOptions::new()
                .write(true)
                .open(self.containerd_pipe.stderr.as_str())
                .await
                .map_err(io_error!(e, "open stdout for read"))?;
            let c_stderr_r = OpenOptions::new()
                .read(true)
                .open(self.containerd_pipe.stderr.as_str())
                .await
                .map_err(io_error!(e, "open stdout for read"))?;
            spawn_copy(
                stream,
                c_stderr,
                Some(move || {
                    drop(c_stderr_r);
                }),
            );
        }
        Ok(())
    }

    fn container_in(&self) -> String {
        self.stdin.clone().unwrap_or_default().to_string()
    }

    fn container_out(&self) -> String {
        self.stdout.clone().unwrap_or_default().to_string()
    }

    fn container_err(&self) -> String {
        self.stderr.clone().unwrap_or_default().to_string()
    }

    async fn new_task_client(address: &str) -> Result<TaskClient> {
        vsock_task_client(address).await
    }

    async fn cleanup_connection(self) {
        Self::remove_port(self).await;
    }
}

impl FromStr for VsockIO {
    type Err = containerd_shim::error::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if let Some(addr_port) = s.strip_prefix(HVSOCK_PREFIX) {
            let (addr, port) = addr_port.split_once(':').unwrap_or_default();
            return Ok(Self {
                sock_path: addr.to_string(),
                port: port.parse::<i32>()?,
            });
        }
        Err(other!("{} doesn't have prefix hvsock", s))
    }
}

impl VSockTransport {
    async fn find_available_port() -> Result<i32> {
        let mut port_map = PORTMAP.lock().await;
        for i in 20000..65536 {
            if port_map.insert(i) {
                return Ok(i);
            }
        }
        Err(other!("no port available in port map"))
    }
    pub async fn remove_port(io: VSockTransport) {
        let mut port_map = PORTMAP.lock().await;
        let _ = [io.stdin, io.stdout, io.stderr]
            .into_iter()
            .flatten()
            .map(|vsock_io| {
                if !port_map.remove(&vsock_io.port) {
                    warn!(
                        "something wrong! could not found port({}) in port map",
                        vsock_io.port
                    )
                }
            });
    }
}

fn spawn_copy<R, W, F>(from: R, to: W, on_close: Option<F>)
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    let mut src = from;
    let mut dst = to;
    tokio::spawn(async move {
        let res = tokio::io::copy(&mut src, &mut dst).await;

        if let Err(e) = res {
            error!("copy io failed {}", e);
        }
        if let Some(f) = on_close {
            f();
        };
    });
}
