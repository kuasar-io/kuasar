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
use containerd_sandbox::error::Result;
use netlink_packet_route::{RouteMessage, RT_TABLE_MAIN};
use serde_derive::{Deserialize, Serialize};

use crate::network::{address::convert_to_ip_address, link::NetworkInterface};

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub device: String,
    #[serde(skip_deserializing)]
    pub source: String,
    // dest in the Route is in the form of "192.168.1.0/24"
    pub dest: String,
    #[serde(default)]
    pub gateway: String,
    #[serde(default)]
    pub scope: u32,
}

impl Route {
    pub fn parse_from_message(msg: RouteMessage, intfs: &[NetworkInterface]) -> Result<Self> {
        if msg.header.table != RT_TABLE_MAIN {
            return Err(anyhow!("ignore routes not in main table").into());
        }
        let mut route = Route {
            scope: msg.header.scope as u32,
            ..Default::default()
        };
        use netlink_packet_route::nlas::route::Nla;
        for nla in msg.nlas.into_iter() {
            match nla {
                Nla::Destination(v) if !v.is_empty() => {
                    let ip = convert_to_ip_address(v)?.to_string();
                    route.dest = format!("{}/{}", ip, msg.header.destination_prefix_length);
                }
                Nla::Source(v) if !v.is_empty() => {
                    route.source = convert_to_ip_address(v)?.to_string();
                }
                Nla::Gateway(v) if !v.is_empty() => {
                    route.gateway = convert_to_ip_address(v)?.to_string();
                }
                Nla::Oif(u) => {
                    intfs
                        .iter()
                        .find(|&x| x.index == u)
                        .map(|x| route.device = x.name.to_string())
                        .ok_or(anyhow!("can not find the device by index {}", u))?;
                }
                _ => {}
            }
        }
        Ok(route)
    }
}
