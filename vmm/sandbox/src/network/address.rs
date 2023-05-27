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
    convert::TryInto,
    fmt::{Debug, Formatter},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use serde_derive::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Deserialize, Serialize)]
pub struct CniIPAddress {
    #[serde(skip_deserializing)]
    pub family: i32,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub mask: String,
}

impl From<&mut CniIPAddress> for String {
    fn from(ip: &mut CniIPAddress) -> String {
        format!("{}/{}", ip.address, ip.mask)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(from = "String")]
#[serde(into = "String")]
pub struct IpNet {
    pub ip: IpAddr,
    pub prefix_len: u8,
}

impl From<String> for IpNet {
    fn from(s: String) -> Self {
        let sp: Vec<&str> = s.split('/').collect();
        let mut ip = IpAddr::from_str("0.0.0.0").unwrap();
        let mut prefix_len = 32u8;
        if !sp.is_empty() {
            if let Ok(i) = IpAddr::from_str(sp[0]) {
                ip = i;
            }
        }
        if sp.len() == 2 {
            if let Ok(s) = sp[1].parse() {
                prefix_len = s;
            }
        };
        Self { ip, prefix_len }
    }
}

impl From<IpNet> for String {
    fn from(ip_net: IpNet) -> String {
        format!("{}/{}", ip_net.addr_string(), ip_net.prefix_len)
    }
}

impl IpNet {
    pub fn new(ip: IpAddr, prefix_len: u8) -> Self {
        Self { ip, prefix_len }
    }

    pub fn addr(&self) -> &IpAddr {
        &self.ip
    }

    pub fn addr_string(&self) -> String {
        self.ip.to_string()
    }

    pub fn netmask(&self) -> IpAddr {
        if self.ip.is_ipv6() {
            let mask = Ipv6Addr::from(self.netmask_u128());
            IpAddr::from(mask)
        } else {
            let mask = Ipv4Addr::from(self.netmask_32());
            IpAddr::from(mask)
        }
    }

    fn netmask_32(&self) -> u32 {
        u32::MAX
            .checked_shl((32 - self.prefix_len) as u32)
            .unwrap_or(u32::MIN)
    }

    fn netmask_u128(&self) -> u128 {
        u128::MAX
            .checked_shl((128 - self.prefix_len) as u32)
            .unwrap_or(u128::MIN)
    }
}

#[derive(Default, Clone, Deserialize, Serialize)]
#[serde(from = "String")]
#[serde(into = "String")]
pub struct MacAddress(pub(crate) Vec<u8>);

impl From<String> for MacAddress {
    fn from(s: String) -> Self {
        Self(
            s.split(':')
                .map(|x| u8::from_str_radix(x, 16).unwrap_or_default())
                .collect(),
        )
    }
}

impl From<MacAddress> for String {
    fn from(val: MacAddress) -> Self {
        val.to_string()
    }
}

impl ToString for MacAddress {
    fn to_string(&self) -> String {
        let mut segs = vec![];
        for u in self.0.as_slice() {
            segs.push(format!("{:02x}", u));
        }
        segs.join(":")
    }
}

impl Debug for MacAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

pub fn convert_to_ip_address(addr: Vec<u8>) -> Result<IpAddr> {
    if addr.len() == 4 {
        let arr: &[u8; 4] = addr.as_slice().try_into().unwrap();
        let address = IpAddr::from(*arr);
        return Ok(address);
    }
    if addr.len() == 16 {
        let arr: &[u8; 16] = addr.as_slice().try_into().unwrap();
        let address = IpAddr::from(*arr);
        return Ok(address);
    }
    Err(anyhow!("ip address vec has length {}", addr.len()).into())
}
