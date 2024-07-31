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
    os::fd::{IntoRawFd, RawFd},
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use containerd_sandbox::{
    error::{Error, Result},
    signal::ExitSignal,
};
use log::{debug, error};
use nix::{
    sys::{
        socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr, VsockAddr},
        time::TimeValLike,
    },
    time::{clock_gettime, ClockId},
    unistd::close,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};
use ttrpc::{context::with_timeout, r#async::Client};
use vmm_common::api::{
    sandbox::{CheckRequest, SyncClockPacket, UpdateInterfacesRequest, UpdateRoutesRequest},
    sandbox_ttrpc::SandboxServiceClient,
};

use crate::network::{NetworkInterface, Route};

const HVSOCK_RETRY_TIMEOUT_IN_MS: u64 = 10;
// TODO: reduce to 10s
const NEW_TTRPC_CLIENT_TIMEOUT: u64 = 45;
const TIME_SYNC_PERIOD: u64 = 60;
const TIME_DIFF_TOLERANCE_IN_MS: u64 = 10;

pub(crate) async fn new_sandbox_client(address: &str) -> Result<SandboxServiceClient> {
    let client = new_ttrpc_client_with_timeout(address, NEW_TTRPC_CLIENT_TIMEOUT).await?;
    Ok(SandboxServiceClient::new(client))
}

async fn new_ttrpc_client_with_timeout(address: &str, t: u64) -> Result<Client> {
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
            // In case that the address doesn't exist, the executed function in this loop are all
            // sync, making the first time of future poll in timeout hang forever. As a result, the
            // timeout will hang too. To solve this, add a async function in this loop or call
            // `tokio::task::yield_now()` to give up current cpu time slice.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    let client = timeout(Duration::from_secs(t), fut)
        .await
        .map_err(|_| anyhow!("{}s timeout connecting socket: {}", t, last_err))?;
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
        stream
            .write_all(format!("CONNECT {}\n", port).as_bytes())
            .await
            .map_err(|e| anyhow!("hvsock connected but failed to write CONNECT: {}", e))?;

        let mut response = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut response)
            .await
            .map_err(|e| anyhow!("CONNECT sent but failed to get response: {}", e))?;
        if response.starts_with("OK") {
            Ok(stream.into_std()?.into_raw_fd())
        } else {
            Err(anyhow!("CONNECT sent but response is not OK: {}", response).into())
        }
    };

    timeout(Duration::from_millis(HVSOCK_RETRY_TIMEOUT_IN_MS), fut)
        .await
        .map_err(|_| anyhow!("hvsock retry {}ms timeout", HVSOCK_RETRY_TIMEOUT_IN_MS))?
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

pub(crate) fn client_sync_clock(
    client: &SandboxServiceClient,
    id: &str,
    exit_signal: Arc<ExitSignal>,
) {
    let id = id.to_string();
    let client = client.clone();
    let tolerance_nanos = Duration::from_millis(TIME_DIFF_TOLERANCE_IN_MS);
    tokio::spawn(async move {
        let fut = async {
            loop {
                tokio::time::sleep(Duration::from_secs(TIME_SYNC_PERIOD)).await;
                if let Err(e) = do_once_sync_clock(&client, tolerance_nanos).await {
                    debug!("sync_clock {}: {:?}", id, e);
                }
            }
        };

        tokio::select! {
            _ = fut => (),
            _ = exit_signal.wait() => {},
        }
    });
}

// Introduce a set of mechanism based on Precision Time Protocol to keep guest clock synchronized
// with host clock periodically.
async fn do_once_sync_clock(
    client: &SandboxServiceClient,
    tolerance_nanos: Duration,
) -> Result<()> {
    let mut req = SyncClockPacket::new();
    let clock_id = ClockId::from_raw(nix::libc::CLOCK_REALTIME);
    req.ClientSendTime = clock_gettime(clock_id)
        .map_err(|e| anyhow!("get current clock: {}", e))?
        .num_nanoseconds();

    let mut p = client
        .sync_clock(with_timeout(Duration::from_secs(1).as_nanos() as i64), &req)
        .await
        .map_err(|e| anyhow!("get guest clock packet: {:?}", e))?;

    p.ServerArriveTime = clock_gettime(clock_id)
        .map_err(|e| anyhow!("get current clock: {}", e))?
        .num_nanoseconds();

    p.Delta = checked_compute_delta(
        p.ClientSendTime,
        p.ClientArriveTime,
        p.ServerSendTime,
        p.ServerArriveTime,
    )?;
    if p.Delta.abs() > tolerance_nanos.as_nanos() as i64 {
        client
            .sync_clock(with_timeout(Duration::from_secs(1).as_nanos() as i64), &p)
            .await
            .map_err(|e| anyhow!("set delta: {:?}", e))?;
    }
    Ok(())
}

// delta = ((c_send - c_arrive) + (s_arrive - s_send)) / 2
fn checked_compute_delta(c_send: i64, c_arrive: i64, s_send: i64, s_arrive: i64) -> Result<i64> {
    let delta_client = c_send
        .checked_sub(c_arrive)
        .ok_or_else(|| anyhow!("integer overflow {} - {}", c_send, c_arrive))?;

    let delta_server = s_arrive
        .checked_sub(s_send)
        .ok_or_else(|| anyhow!("integer overflow {} - {}", s_arrive, s_send))?;

    let delta_sum = delta_client
        .checked_add(delta_server)
        .ok_or_else(|| anyhow!("integer overflow {} + {}", delta_client, delta_server))?;

    let delta = delta_sum
        .checked_div(2)
        .ok_or_else(|| anyhow!("integer overflow {} / 2", delta_sum))?;

    Ok(delta)
}

#[cfg(test)]
mod tests {
    use crate::client::{checked_compute_delta, new_ttrpc_client_with_timeout};

    #[tokio::test]
    async fn test_new_ttrpc_client_timeout() {
        // Expect new_ttrpc_client would return timeout error, instead of blocking.
        assert!(new_ttrpc_client_with_timeout("hvsock://fake.sock:1024", 1)
            .await
            .is_err());
    }

    #[test]
    fn test_checked_compute_delta() {
        let c_send = 231;
        let c_arrive = 135;
        let s_send = 137;
        let s_arrive = 298;

        let expect_delta = 128;
        let actual_delta = checked_compute_delta(c_send, c_arrive, s_send, s_arrive).unwrap();
        assert_eq!(expect_delta, actual_delta);
    }
}
