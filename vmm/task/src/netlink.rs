// Copyright (c) 2021 Kata Maintainers
//
// SPDX-License-Identifier: Apache-2.0
//

use std::{
    convert::TryFrom,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    ops::Deref,
    str::FromStr,
};

use containerd_shim::{
    error::{Error, Result},
    other, other_error,
    protos::protobuf::EnumOrUnknown,
};
use futures::{future, TryStreamExt};
use ipnetwork::{IpNetwork, Ipv4Network, Ipv6Network};
use netlink_packet_route as packet;
use nix::errno::Errno;
use rtnetlink::{new_connection, IpVersion};
use vmm_common::api::sandbox::{IPAddress, IPFamily, Interface, Route};

/// Search criteria to use when looking for a link in `find_link`.
pub enum LinkFilter<'a> {
    /// Find by link name.
    Name(&'a str),
    /// Find by link index.
    Index(u32),
    /// Find by MAC address.
    Address(&'a str),
}

impl fmt::Display for LinkFilter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkFilter::Name(name) => write!(f, "Name: {}", name),
            LinkFilter::Index(idx) => write!(f, "Index: {}", idx),
            LinkFilter::Address(addr) => write!(f, "Address: {}", addr),
        }
    }
}

/// A filter to query addresses.
pub enum AddressFilter {
    /// Return addresses that belong to the given interface.
    LinkIndex(u32),
    /// Get addresses with the given prefix.
    #[allow(dead_code)]
    IpAddress(IpAddr),
}

/// A high level wrapper for netlink (and `rtnetlink` crate) for use by the Agent's RPC.
/// It is expected to be consumed by the `AgentService`, so it operates with protobuf
/// structures directly for convenience.
pub struct Handle {
    handle: rtnetlink::Handle,
}

impl Handle {
    pub(crate) fn new() -> Result<Handle> {
        let (conn, handle, _) =
            new_connection().map_err(other_error!(e, "failed to new netlink connection"))?;
        tokio::spawn(conn);

        Ok(Handle { handle })
    }

    pub(crate) async fn enable_lo(&self) -> Result<()> {
        let link = self.find_link(LinkFilter::Name("lo")).await?;
        self.enable_link(link.index(), true).await?;
        Ok(())
    }

    pub async fn update_interfaces(&mut self, intfs: Vec<Interface>) -> Result<()> {
        for intf in intfs {
            // The reliable way to find link is using hardware address
            // as filter. However, hardware filter might not be supported
            // by netlink, we may have to dump link list and the find the
            // target link. filter using name or family is supported, but
            // we cannot use that to find target link.
            // let's try if hardware address filter works. -_-
            let link = self.find_link(LinkFilter::Address(&intf.hwAddr)).await?;

            // Bring down interface if it is UP
            if link.is_up() {
                self.enable_link(link.index(), false).await?;
            }

            // Delete all addresses associated with the link
            let addresses = self
                .list_addresses(AddressFilter::LinkIndex(link.index()))
                .await?;
            self.delete_addresses(addresses).await?;

            // Add new ip addresses from request
            for ip_address in &intf.IPAddresses {
                let ip = IpAddr::from_str(&ip_address.address).map_err(other_error!(
                    e,
                    format!("invalid ip address: {}", ip_address.address)
                ))?;
                let mask = ip_address.mask.parse::<u8>()?;

                self.add_addresses(
                    link.index(),
                    std::iter::once(
                        IpNetwork::new(ip, mask).map_err(other_error!(e, "invalid ip and mask"))?,
                    ),
                )
                .await?;
            }

            // Update link
            let mut request = self.handle.link().set(link.index());
            request.message_mut().header = link.header.clone();

            // if there are more than one interfaces in the pod, when the kernel boot up,
            // it will give the name of the interfaces "ethX",
            // where X is the order the driver register its device to network.
            // so there are opportunities that the interface "eth1" (or "veth1" or anything else)
            // on the host become "eth0" in the guest as its original status.
            // in that case we have to change its name to a temporary name, to avoid name conflict.
            // the temporary name will be changed to the name on the host after updating that interface.
            if link.name() != intf.name.as_str() {
                let existed_link = self.find_link(LinkFilter::Name(&intf.name)).await;
                if let Ok(l) = existed_link {
                    let mut request = self.handle.link().set(l.index());
                    request.message_mut().header = l.header.clone();
                    request
                        .name(format!("{}-tmp", l.name()))
                        .execute()
                        .await
                        .map_err(other_error!(e, "failed to execute netlink request"))?;
                }
            }
            request
                .mtu(intf.mtu as _)
                .name(intf.name.clone())
                .arp(intf.raw_flags & libc::IFF_NOARP as u32 == 0)
                .up()
                .execute()
                .await
                .map_err(other_error!(e, "failed to execute netlink request"))?;
        }
        Ok(())
    }

    async fn find_link(&self, filter: LinkFilter<'_>) -> Result<Link> {
        let request = self.handle.link().get();

        let filtered = match filter {
            LinkFilter::Name(name) => request.match_name(name.to_owned()),
            LinkFilter::Index(index) => request.match_index(index),
            _ => request, // Post filters
        };

        let mut stream = filtered.execute();

        let next = if let LinkFilter::Address(addr) = filter {
            use packet::link::nlas::Nla;

            let mac_addr = parse_mac_address(addr)?;

            // Hardware filter might not be supported by netlink,
            // we may have to dump link list and then find the target link.
            stream
                .try_filter(|f| {
                    let result = f.nlas.iter().any(|n| match n {
                        Nla::Address(data) => data.eq(&mac_addr),
                        _ => false,
                    });

                    future::ready(result)
                })
                .try_next()
                .await
                .map_err(other_error!(e, ""))?
        } else {
            stream.try_next().await.map_err(other_error!(e, ""))?
        };

        next.map(|msg| msg.into())
            .ok_or_else(|| other!("Link not found ({})", filter))
    }

    pub async fn enable_link(&self, link_index: u32, up: bool) -> Result<()> {
        let link_req = self.handle.link().set(link_index);
        let set_req = if up { link_req.up() } else { link_req.down() };
        set_req
            .execute()
            .await
            .map_err(other_error!(e, "failed to enable link"))?;
        Ok(())
    }

    async fn list_addresses<F>(&self, filter: F) -> Result<Vec<Address>>
    where
        F: Into<Option<AddressFilter>>,
    {
        let mut request = self.handle.address().get();

        if let Some(filter) = filter.into() {
            request = match filter {
                AddressFilter::LinkIndex(index) => request.set_link_index_filter(index),
                AddressFilter::IpAddress(addr) => request.set_address_filter(addr),
            };
        };

        let list = request
            .execute()
            .try_filter_map(|msg| future::ready(Ok(Some(Address(msg))))) // Map message to `Address`
            .try_collect()
            .await
            .map_err(other_error!(e, "failed to execute list address"))?;
        Ok(list)
    }

    async fn add_addresses<I>(&mut self, index: u32, list: I) -> Result<()>
    where
        I: IntoIterator<Item = IpNetwork>,
    {
        for net in list.into_iter() {
            self.handle
                .address()
                .add(index, net.ip(), net.prefix())
                .execute()
                .await
                .map_err(other_error!(
                    e,
                    format!("Failed to add address {}", net.ip())
                ))?;
        }

        Ok(())
    }

    async fn delete_addresses<I>(&mut self, list: I) -> Result<()>
    where
        I: IntoIterator<Item = Address>,
    {
        for addr in list.into_iter() {
            self.handle
                .address()
                .del(addr.0)
                .execute()
                .await
                .map_err(other_error!(e, "failed to del address"))?;
        }

        Ok(())
    }

    pub async fn update_routes<I>(&mut self, list: I) -> Result<()>
    where
        I: IntoIterator<Item = Route>,
    {
        let old_routes = self.query_routes(None).await?;

        self.delete_routes(old_routes).await?;

        self.add_routes(list).await?;

        Ok(())
    }

    async fn query_routes(
        &self,
        ip_version: Option<IpVersion>,
    ) -> Result<Vec<packet::RouteMessage>> {
        let list = if let Some(ip_version) = ip_version {
            self.handle
                .route()
                .get(ip_version)
                .execute()
                .try_collect()
                .await
                .map_err(other_error!(e, "failed to query routes"))?
        } else {
            // These queries must be executed sequentially, otherwise
            // it'll throw "Device or resource busy (os error 16)"
            let routes4 = self
                .handle
                .route()
                .get(IpVersion::V4)
                .execute()
                .try_collect::<Vec<_>>()
                .await
                .map_err(other_error!(e, "failed to query IPV4 routes "))?;

            let routes6 = self
                .handle
                .route()
                .get(IpVersion::V6)
                .execute()
                .try_collect::<Vec<_>>()
                .await
                .map_err(other_error!(e, "failed to query IPV6 routes"))?;

            [routes4, routes6].concat()
        };

        Ok(list)
    }

    /// Adds a list of routes from iterable object `I`.
    /// It can accept both a collection of routes or a single item (via `iter::once()`).
    /// It'll also take care of proper order when adding routes (gateways first, everything else after).
    async fn add_routes<I>(&mut self, list: I) -> Result<()>
    where
        I: IntoIterator<Item = Route>,
    {
        // Split the list so we add routes with no gateway first.
        // Note: `partition_in_place` is a better fit here, since it reorders things inplace (instead of
        // allocating two separate collections), however it's not yet in stable Rust.
        let (a, b): (Vec<Route>, Vec<Route>) = list.into_iter().partition(|p| p.gateway.is_empty());
        let list = a.iter().chain(&b);

        for route in list {
            let link = self.find_link(LinkFilter::Name(&route.device)).await?;

            const MAIN_TABLE: u8 = packet::constants::RT_TABLE_MAIN;
            const UNICAST: u8 = packet::constants::RTN_UNICAST;
            const BOOT_PROT: u8 = packet::constants::RTPROT_BOOT;

            let scope = route.scope as u8;

            use packet::nlas::route::Nla;

            // Build a common indeterminate ip request
            let request = self
                .handle
                .route()
                .add()
                .table(MAIN_TABLE)
                .kind(UNICAST)
                .protocol(BOOT_PROT)
                .scope(scope);

            // `rtnetlink` offers a separate request builders for different IP versions (IP v4 and v6).
            // This if branch is a bit clumsy because it does almost the same.
            if route.family.enum_value_or_default() == IPFamily::v6 {
                let dest_addr = if !route.dest.is_empty() {
                    Ipv6Network::from_str(&route.dest)
                        .map_err(other_error!(e, "failed parse ipv6 network"))?
                } else {
                    Ipv6Network::new(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 0)
                        .map_err(other_error!(e, "failed new ipv6 network"))?
                };

                // Build IP v6 request
                let mut request = request
                    .v6()
                    .destination_prefix(dest_addr.ip(), dest_addr.prefix())
                    .output_interface(link.index());

                if !route.source.is_empty() {
                    let network = Ipv6Network::from_str(&route.source)
                        .map_err(other_error!(e, "failed parse ipv6 network"))?;
                    if network.prefix() > 0 {
                        request = request.source_prefix(network.ip(), network.prefix());
                    } else {
                        request
                            .message_mut()
                            .nlas
                            .push(Nla::PrefSource(network.ip().octets().to_vec()));
                    }
                }

                if !route.gateway.is_empty() {
                    let ip = Ipv6Addr::from_str(&route.gateway)
                        .map_err(other_error!(e, "failed parse ipv6 network"))?;
                    request = request.gateway(ip);
                }

                if let Err(rtnetlink::Error::NetlinkError(message)) = request.execute().await {
                    if Errno::from_i32(message.code.abs()) != Errno::EEXIST {
                        return Err(other!(
                            "Failed to add IP v6 route (src: {}, dst: {}, gtw: {},Err: {})",
                            route.source,
                            route.dest,
                            route.gateway,
                            message
                        ));
                    }
                }
            } else {
                let dest_addr = if !route.dest.is_empty() {
                    Ipv4Network::from_str(&route.dest)
                        .map_err(other_error!(e, "failed to parse ipv4 network"))?
                } else {
                    Ipv4Network::new(Ipv4Addr::new(0, 0, 0, 0), 0)
                        .map_err(other_error!(e, "failed to new ipv4 network"))?
                };

                // Build IP v4 request
                let mut request = request
                    .v4()
                    .destination_prefix(dest_addr.ip(), dest_addr.prefix())
                    .output_interface(link.index());

                if !route.source.is_empty() {
                    let network = Ipv4Network::from_str(&route.source)
                        .map_err(other_error!(e, "failed to parse ipv4 network"))?;
                    if network.prefix() > 0 {
                        request = request.source_prefix(network.ip(), network.prefix());
                    } else {
                        request
                            .message_mut()
                            .nlas
                            .push(Nla::PrefSource(network.ip().octets().to_vec()));
                    }
                }

                if !route.gateway.is_empty() {
                    let ip = Ipv4Addr::from_str(&route.gateway).map_err(other_error!(
                        e,
                        format!("failed to parse gateway({})", route.gateway)
                    ))?;
                    request = request.gateway(ip);
                }

                if let Err(rtnetlink::Error::NetlinkError(message)) = request.execute().await {
                    if Errno::from_i32(message.code.abs()) != Errno::EEXIST {
                        return Err(other!(
                            "Failed to add IP v4 route (src: {}, dst: {}, gtw: {},Err: {})",
                            route.source,
                            route.dest,
                            route.gateway,
                            message
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    async fn delete_routes<I>(&mut self, routes: I) -> Result<()>
    where
        I: IntoIterator<Item = packet::RouteMessage>,
    {
        for route in routes.into_iter() {
            if route.header.protocol == packet::constants::RTPROT_KERNEL {
                continue;
            }

            let index = if let Some(index) = route.output_interface() {
                index
            } else {
                continue;
            };

            let link = self.find_link(LinkFilter::Index(index)).await?;

            let name = link.name();
            if name.contains("lo") || name.contains("::1") {
                continue;
            }

            self.handle
                .route()
                .del(route)
                .execute()
                .await
                .map_err(other_error!(e, "failed to delete routes"))?;
        }

        Ok(())
    }
}

/// Wraps external type with the local one, so we can implement various extensions and type conversions.
struct Link(packet::LinkMessage);

impl Link {
    /// If name.
    fn name(&self) -> String {
        use packet::nlas::link::Nla;
        self.nlas
            .iter()
            .find_map(|n| {
                if let Nla::IfName(name) = n {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Extract Mac address.
    #[allow(dead_code)]
    fn address(&self) -> String {
        use packet::nlas::link::Nla;
        self.nlas
            .iter()
            .find_map(|n| {
                if let Nla::Address(data) = n {
                    format_address(data).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Returns whether the link is UP
    fn is_up(&self) -> bool {
        self.header.flags & packet::rtnl::constants::IFF_UP > 0
    }

    fn index(&self) -> u32 {
        self.header.index
    }

    #[allow(dead_code)]
    fn mtu(&self) -> Option<u64> {
        use packet::nlas::link::Nla;
        self.nlas.iter().find_map(|n| {
            if let Nla::Mtu(mtu) = n {
                Some(*mtu as u64)
            } else {
                None
            }
        })
    }
}

impl From<packet::LinkMessage> for Link {
    fn from(msg: packet::LinkMessage) -> Self {
        Link(msg)
    }
}

impl Deref for Link {
    type Target = packet::LinkMessage;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct Address(packet::AddressMessage);

impl TryFrom<Address> for IPAddress {
    type Error = containerd_shim::error::Error;

    fn try_from(value: Address) -> Result<Self> {
        let family = if value.is_ipv6() {
            IPFamily::v4
        } else {
            IPFamily::v6
        };

        let mut address = value.address();
        if address.is_empty() {
            address = value.local();
        }

        let mask = format!("{}", value.0.header.prefix_len);

        Ok(IPAddress {
            family: EnumOrUnknown::from(family),
            address,
            mask,
            ..Default::default()
        })
    }
}

impl Address {
    fn is_ipv6(&self) -> bool {
        self.0.header.family == packet::constants::AF_INET6 as u8
    }

    #[allow(dead_code)]
    fn prefix(&self) -> u8 {
        self.0.header.prefix_len
    }

    fn address(&self) -> String {
        use packet::nlas::address::Nla;
        self.0
            .nlas
            .iter()
            .find_map(|n| {
                if let Nla::Address(data) = n {
                    format_address(data).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    fn local(&self) -> String {
        use packet::nlas::address::Nla;
        self.0
            .nlas
            .iter()
            .find_map(|n| {
                if let Nla::Local(data) = n {
                    format_address(data).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
}

fn format_address(data: &[u8]) -> Result<String> {
    match data.len() {
        4 => {
            // IP v4
            Ok(format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3]))
        }
        6 => {
            // Mac address
            Ok(format!(
                "{:0>2X}:{:0>2X}:{:0>2X}:{:0>2X}:{:0>2X}:{:0>2X}",
                data[0], data[1], data[2], data[3], data[4], data[5]
            ))
        }
        16 => {
            // IP v6
            let octets =
                <[u8; 16]>::try_from(data).map_err(other_error!(e, "invalid ipv6 address"))?;
            Ok(Ipv6Addr::from(octets).to_string())
        }
        _ => Err(other!("Unsupported address length: {}", data.len())),
    }
}

fn parse_mac_address(addr: &str) -> Result<[u8; 6]> {
    let mut split = addr.splitn(6, ':');

    // Parse single Mac address block
    let mut parse_next = || -> Result<u8> {
        let v = u8::from_str_radix(
            split
                .next()
                .ok_or_else(|| other!("Invalid MAC address {}", addr))?,
            16,
        )?;
        Ok(v)
    };

    // Parse all 6 blocks
    let arr = [
        parse_next()?,
        parse_next()?,
        parse_next()?,
        parse_next()?,
        parse_next()?,
        parse_next()?,
    ];

    Ok(arr)
}
