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

use netlink_packet_route::AddressFamily;
use protobuf::{EnumOrUnknown, SpecialFields};
use vmm_common::api::sandbox::{IPAddress, IPFamily, Interface, Route};

use crate::network::{IpNet, NetworkInterface};

impl From<&NetworkInterface> for Interface {
    fn from(interface: &NetworkInterface) -> Self {
        Self {
            device: interface.name.to_string(),
            name: interface.name.to_string(),
            IPAddresses: interface.ip_addresses.iter().map(|i| i.into()).collect(),
            mtu: interface.mtu as u64,
            hwAddr: interface.mac_address.to_string(),
            raw_flags: interface.flags,
            type_: "".to_string(),
            special_fields: SpecialFields::default(),
        }
    }
}

impl From<&IpNet> for IPAddress {
    fn from(ip: &IpNet) -> Self {
        Self {
            family: EnumOrUnknown::from(if ip.ip.is_ipv6() {
                IPFamily::v6
            } else {
                IPFamily::v4
            }),
            address: ip.addr_string(),
            mask: ip.prefix_len.to_string(),
            special_fields: SpecialFields::default(),
        }
    }
}

impl From<&crate::network::Route> for Route {
    fn from(r: &crate::network::Route) -> Self {
        Self {
            dest: r.dest.to_string(),
            gateway: r.gateway.to_string(),
            device: r.device.to_string(),
            source: r.source.to_string(),
            scope: r.scope as u32,
            family: EnumOrUnknown::from(match AddressFamily::from(r.family) {
                AddressFamily::Inet => IPFamily::v4,
                AddressFamily::Inet6 => IPFamily::v6,
                _ => IPFamily::default(),
            }),
            flags: r.flags,
            special_fields: Default::default(),
        }
    }
}
