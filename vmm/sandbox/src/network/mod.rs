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
    fmt::{Debug, Display, Formatter},
    os::unix::prelude::AsRawFd,
    path::Path,
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use futures_util::TryStreamExt;
use log::{debug, error, info, warn};
use nix::{
    fcntl::OFlag,
    sched::{setns, CloneFlags},
    sys::stat::Mode,
};
use rtnetlink::{new_connection, Handle, IpVersion};
use serde_derive::{Deserialize, Serialize};
use tokio::task::spawn_blocking;

pub use crate::network::{address::IpNet, link::NetworkInterface, route::Route};
use crate::{network::link::LinkType, sandbox::KuasarSandbox, utils::safe_open_file, vm::VM};

pub mod address;
mod convert;
pub mod link;
mod netlink;
pub mod route;

#[derive(Debug, Serialize, Deserialize)]
pub struct Network {
    pub(crate) config: NetworkConfig,
    pub(crate) intfs: Vec<NetworkInterface>,
    routes: Vec<Route>,
}

async fn get_route(
    ip_version: IpVersion,
    handle: &Handle,
    intfs: &[NetworkInterface],
    routes: &mut Vec<Route>,
) -> Result<()> {
    let mut route_msgs = handle.route().get(ip_version).execute();
    while let Some(route_msg) = route_msgs.try_next().await.map_err(|e| anyhow!("{}", e))? {
        let route_res = Route::parse_from_message(route_msg, intfs);
        match route_res {
            Ok(r) => {
                routes.push(r);
            }
            Err(e) => {
                // ignore those routes that can not be parsed
                debug!("can not parse the route message to route {}", e);
            }
        }
    }

    Ok(())
}

impl Network {
    pub async fn new(config: NetworkConfig) -> Result<Self> {
        debug!("create network with config: {:?}", config);
        let sandbox_id = config.sandbox_id.to_string();
        let network = Self::new_from_netns(config).await?;
        info!("network for sandbox {}: {:?}", sandbox_id, &network);
        Ok(network)
    }

    pub async fn new_from_netns(config: NetworkConfig) -> Result<Self> {
        Self::new_in_netns(config).await
    }

    async fn new_in_netns(config: NetworkConfig) -> Result<Self> {
        let handle = create_netlink_handle(&config.netns).await?;

        // get all links from netns
        let mut links = handle.link().get().execute();
        let mut intfs = vec![];
        while let Some(msg) = links.try_next().await.map_err(|e| anyhow!(e))? {
            let network_interface = match NetworkInterface::parse_from_message(
                msg,
                &config.netns,
                config.queue,
                &handle,
            )
            .await
            {
                Ok(interface) => interface,
                Err(e) => {
                    warn!("failed to parse network interface: {}, ignore it", e);
                    continue;
                }
            };
            if let LinkType::Loopback = network_interface.r#type {
                continue;
            }
            intfs.push(network_interface);
        }
        let intfs = Self::filter_intfs(intfs);

        // get all routes from netns
        let mut routes = vec![];
        get_route(IpVersion::V4, &handle, &intfs, &mut routes).await?;
        get_route(IpVersion::V6, &handle, &intfs, &mut routes).await?;

        Ok(Network {
            config,
            intfs,
            routes,
        })
    }

    pub async fn attach_to<V: VM>(self, sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        let netns = self.config.netns.to_string();
        let mut me = self;
        for intf in &mut me.intfs {
            intf.prepare_attaching(&netns).await?;
            intf.attach_to(sandbox).await?;
        }
        sandbox.network = Some(me);
        Ok(())
    }

    pub async fn destroy(&mut self) {
        for intf in &mut self.intfs {
            if let Err(e) = intf.after_detach(&self.config.netns).await {
                error!(
                    "failed to recycle interface {} when destroying, err {:?}",
                    intf.name, e
                );
            }
        }
    }

    pub fn interfaces(&self) -> &Vec<NetworkInterface> {
        self.intfs.as_ref()
    }

    pub fn routes(&self) -> &Vec<Route> {
        self.routes.as_ref()
    }

    fn filter_intfs(intfs: Vec<NetworkInterface>) -> Vec<NetworkInterface> {
        intfs
            .into_iter()
            .filter(|intf| match intf.r#type {
                LinkType::Veth => true,
                LinkType::VhostUser(_) => true,
                LinkType::Physical(_, _) => true,
                LinkType::Tap => true,
                LinkType::Loopback => {
                    // do we have to drop loopback?
                    true
                }
                _ => false,
            })
            .collect::<Vec<NetworkInterface>>()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub(crate) netns: String,
    pub(crate) sandbox_id: String,
    pub(crate) queue: u32,
}

async fn run_in_new_netns<P: AsRef<Path>, F, T>(netns: P, f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let netns_fd = safe_open_file(netns.as_ref(), OFlag::O_CLOEXEC, Mode::empty())
        .map_err(|e| anyhow!("failed to open netns: {}", e))?;
    let netns_handle = spawn_blocking(move || {
        let old_netns_fd =
            safe_open_file("/proc/self/ns/net", OFlag::O_CLOEXEC, Mode::empty()).unwrap();
        setns(netns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET).unwrap();
        let res = f();
        setns(old_netns_fd.as_raw_fd(), CloneFlags::CLONE_NEWNET).unwrap();
        res
    });
    let res = netns_handle
        .await
        .map_err(|e| anyhow!("failed to wait for the netns: {}", e))?;
    Ok(res)
}

pub(crate) async fn create_netlink_handle(netns: &str) -> Result<Handle> {
    let (connection, handle, _) = if netns.is_empty() {
        new_connection()?
    } else {
        run_in_new_netns(netns, new_connection).await??
    };
    tokio::spawn(connection);
    Ok(handle)
}

pub async fn execute_in_netns(netns: &str, mut cmd: std::process::Command) -> Result<String> {
    let output = if !netns.is_empty() {
        run_in_new_netns(netns, move || cmd.output()).await?
    } else {
        cmd.output()
    }?;
    if !output.status.success() {
        Err(anyhow!(
            "failed to execute command, command return {:?}, stdout: {}, stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        )
        .into())
    } else {
        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| anyhow!("failed to execute command: {}", e))?;
        Ok(stdout)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetType {
    Tap,
    MacVtap,
    IpVtap,
    VethTap,
    VhostUser,
}

impl Display for NetType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match &self {
            NetType::Tap => "tap".to_string(),
            NetType::MacVtap => "tap".to_string(),
            NetType::IpVtap => "tap".to_string(),
            NetType::VethTap => "tap".to_string(),
            NetType::VhostUser => "vhost-user".to_string(),
        };
        write!(f, "{}", s)
    }
}

#[cfg(test)]
mod tests {
    use crate::network::{Network, NetworkConfig};

    #[tokio::test]
    async fn test_new() {
        let network = Network::new_from_netns(NetworkConfig {
            netns: "".to_string(),
            sandbox_id: "".to_string(),
            queue: 1,
        })
        .await
        .unwrap();
        println!("network init succeed");
        for en in network.intfs {
            println!("endpoint: {:?}", en);
        }
        for route in network.routes {
            println!("route: {:?}", route);
        }
    }
}
