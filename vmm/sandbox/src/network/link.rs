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
    ffi::CStr,
    os::unix::{
        fs::OpenOptionsExt,
        prelude::{AsRawFd, OwnedFd},
    },
    path::Path,
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use futures_util::TryStreamExt;
use libc::{IFF_MULTI_QUEUE, IFF_NO_PI, IFF_TAP, IFF_VNET_HDR};
use netlink_packet_route::{
    link::nlas::{Info, InfoData, InfoIpVlan, InfoKind, InfoMacVlan, InfoMacVtap, InfoVlan},
    nlas::link::InfoVxlan,
    LinkMessage,
};
use nix::{
    ioctl_read_bad, ioctl_write_ptr_bad, libc,
    sys::socket::{socket, AddressFamily, SockFlag, SockType},
    unistd::close,
};
use rtnetlink::Handle;
use serde_derive::{Deserialize, Serialize};

use crate::{
    device::{DeviceInfo, PhysicalDeviceInfo, TapDeviceInfo, VhostUserDeviceInfo},
    network::{
        address::{convert_to_ip_address, CniIPAddress, IpNet, MacAddress},
        create_netlink_handle, execute_in_netns, run_in_new_netns,
    },
    sandbox::KuasarSandbox,
    utils::write_file_async,
    vm::VM,
};

const DEVICE_DRIVER_VFIO: &str = "vfio-pci";

#[allow(dead_code)]
const SIOCETHTOOL: u64 = 0x8946;
#[allow(dead_code)]
const ETHTOOL_GDRVINFO: u32 = 0x00000003;

const TUNSETIFF: u64 = 0x400454ca;
const TUNSETPERSIST: u64 = 0x400454cb;

ioctl_write_ptr_bad!(ioctl_tun_set_iff, TUNSETIFF, ifreq);
ioctl_write_ptr_bad!(ioctl_tun_set_persist, TUNSETPERSIST, u64);

#[allow(dead_code)]
#[repr(C)]
pub struct Ifreq {
    ifr_name: [u8; 16],
    ifr_data: *mut libc::c_void,
}

#[allow(dead_code)]
#[repr(C)]
pub struct EthtoolDrvInfo {
    cmd: u32,
    driver: [u8; 32],
    version: [u8; 32],
    fw_version: [u8; 32],
    bus_info: [u8; 32],
    erom_version: [u8; 32],
    reserved2: [u8; 12],
    n_priv_flags: u32,
    n_stats: u32,
    testinfo_len: u32,
    eedump_len: u32,
    regdump_len: u32,
}

ioctl_read_bad!(ioctl_get_drive_info, SIOCETHTOOL, Ifreq);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum LinkType {
    Unkonwn,
    Bridge,
    Veth,
    Vlan(u16),
    Vxlan(u32),
    Bond,
    Ipvlan(u16),
    Macvlan(u32),
    Macvtap(u32),
    Iptun,
    Tun,
    VhostUser(String),
    Physical(String, String),
    Tap,
    Loopback,
}

impl Default for LinkType {
    fn default() -> Self {
        Self::Unkonwn
    }
}

impl ToString for LinkType {
    fn to_string(&self) -> String {
        match &self {
            LinkType::Unkonwn => "".to_string(),
            LinkType::Bridge => "bridge".to_string(),
            LinkType::Veth => "veth".to_string(),
            LinkType::Vlan(_) => "vlan".to_string(),
            LinkType::Vxlan(_) => "vxlan".to_string(),
            LinkType::Bond => "bond".to_string(),
            LinkType::Ipvlan(_) => "ipvlan".to_string(),
            LinkType::Macvlan(_) => "macvlan".to_string(),
            LinkType::Macvtap(_) => "macvtap".to_string(),
            LinkType::Iptun => "iptun".to_string(),
            LinkType::Tun => "tun".to_string(),
            LinkType::VhostUser(_) => "vhostuser".to_string(),
            LinkType::Physical(_, _) => "physical".to_string(),
            LinkType::Tap => "tap".to_string(),
            LinkType::Loopback => "loopback".to_string(),
        }
    }
}

impl From<InfoData> for LinkType {
    fn from(d: InfoData) -> Self {
        match d {
            InfoData::Bridge(_) => return Self::Bridge,
            InfoData::Tun(_) => return Self::Tun,
            InfoData::Vlan(infos) => {
                if let Some(InfoVlan::Id(i)) = infos.first() {
                    return Self::Vlan(*i);
                }
            }
            InfoData::Veth(_) => return LinkType::Veth,
            InfoData::Vxlan(infos) => {
                if let Some(InfoVxlan::Id(i)) = infos.first() {
                    return Self::Vxlan(*i);
                }
            }
            InfoData::Bond(_) => return Self::Bond,
            InfoData::IpVlan(infos) => {
                if let Some(InfoIpVlan::Mode(i)) = infos.first() {
                    return Self::Ipvlan(*i);
                }
            }
            InfoData::MacVlan(infos) => {
                if let Some(InfoMacVlan::Mode(i)) = infos.first() {
                    return Self::Macvlan(*i);
                }
            }
            InfoData::MacVtap(infos) => {
                if let Some(InfoMacVtap::Mode(i)) = infos.first() {
                    return Self::Macvtap(*i);
                }
            }
            InfoData::IpTun(_) => return Self::Iptun,
            _ => return Self::Unkonwn,
        }
        Self::Unkonwn
    }
}

#[derive(Default, Debug, Deserialize, Serialize)]
pub struct NetworkInterface {
    #[serde(default)]
    pub device: String,
    #[serde(skip)]
    pub r#type: LinkType,
    #[serde(skip)]
    pub index: u32,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "IPAddresses")]
    pub cni_ip_addresses: Vec<CniIPAddress>,
    #[serde(skip)]
    pub ip_addresses: Vec<IpNet>,
    #[serde(rename = "hwAddr")]
    pub mac_address: MacAddress,
    #[serde(default)]
    pub mtu: u32,
    #[serde(rename = "rawFlags", skip)]
    pub flags: u32,
    #[serde(skip)]
    pub alias: String,
    #[serde(rename = "pciAddr", skip)]
    pub pci_address: String,
    #[serde(rename = "linkType", skip)]
    pub cni_link_type: String,
    #[serde(rename = "vhostUserSocket")]
    pub vhost_user_socket: String,
    #[serde(skip)]
    pub twin: Option<Box<NetworkInterface>>,
    #[serde(skip)]
    pub fds: Vec<OwnedFd>,
    #[serde(skip)]
    pub queue: u32,
}

impl NetworkInterface {
    pub async fn parse_from_message(
        msg: LinkMessage,
        netns: &str,
        queue: u32,
        handle: &Handle,
    ) -> Result<Self> {
        let mut intf = NetworkInterface {
            flags: msg.header.flags,
            index: msg.header.index,
            ..Default::default()
        };
        for nla in msg.nlas.into_iter() {
            use netlink_packet_route::nlas::link::Nla;
            match nla {
                Nla::Info(infos) => {
                    for info in infos {
                        if let Info::Data(d) = info {
                            intf.r#type = d.into();
                        } else if let Info::Kind(InfoKind::Veth) = info {
                            // for veth, there is no Info::Data, but SlaveKind and SlaveData
                            // so we have to get the type from Info::Kind
                            intf.queue = queue;
                            intf.r#type = LinkType::Veth;
                        }
                    }
                }
                Nla::IfName(s) => {
                    intf.name = s;
                }
                Nla::IfAlias(a) => intf.alias = a,
                Nla::Mtu(m) => intf.mtu = m,
                Nla::Address(u) => intf.mac_address = MacAddress(u),
                _ => {}
            }
        }
        let mut addresses = handle
            .address()
            .get()
            .set_link_index_filter(msg.header.index)
            .execute();
        while let Some(msg) = addresses.try_next().await.map_err(|e| anyhow!(e))? {
            use netlink_packet_route::nlas::address::Nla;
            for nla in msg.nlas.into_iter() {
                if let Nla::Address(addr) = nla {
                    let address = convert_to_ip_address(addr)?;
                    let mask_len = msg.header.prefix_len;
                    if address.is_loopback() {
                        intf.r#type = LinkType::Loopback;
                    }
                    intf.ip_addresses.push(IpNet::new(address, mask_len));
                }
            }
        }
        // find the pci device for unknown type interface, maybe it is a physical interface.
        if let LinkType::Unkonwn = intf.r#type {
            // only search those with ip addresses
            if !intf.ip_addresses.is_empty() {
                let if_name = intf.name.to_string();
                let bdf = if !netns.is_empty() {
                    run_in_new_netns(netns, move || get_bdf_for_eth(&if_name)).await??
                } else {
                    get_bdf_for_eth(&if_name)?
                };
                let driver = get_pci_driver(&bdf).await?;
                intf.r#type = LinkType::Physical(bdf, driver);
            }
        }
        Ok(intf)
    }

    pub async fn init_cni_interface(&mut self) -> Result<()> {
        // TODO: cni_link_type to LinkType
        // TODO: Impl From<String> for LinkType
        if !self.vhost_user_socket.is_empty() {
            self.r#type = LinkType::VhostUser(self.vhost_user_socket.clone());
        } else {
            self.r#type = LinkType::Tap;
        }

        // TODO: impl Map ?
        let mut ip_addresses: Vec<IpNet> = vec![];
        // why &mut
        for ip in &mut self.cni_ip_addresses {
            let ip_str: String = ip.into();
            ip_addresses.push(IpNet::from(ip_str));
        }
        self.ip_addresses = ip_addresses;
        Ok(())
    }

    pub async fn prepare_attaching(&mut self, netns: &str) -> Result<()> {
        match &self.r#type {
            LinkType::Veth => {
                let handle = create_netlink_handle(netns).await?;
                let tap_name = format!("tap_kuasar_{}", self.index);
                let tap_intf =
                    create_tap_in_netns(netns, &tap_name, self.queue, self.mtu, &handle).await?;
                tap_intf.add_qdisc_ingress(netns, &handle).await?;
                self.add_qdisc_ingress(netns, &handle).await?;
                tap_intf.add_redirect_tc_filter(netns, &self.name).await?;
                self.add_redirect_tc_filter(netns, &tap_intf.name).await?;
                self.twin = Some(Box::new(tap_intf));
            }
            LinkType::Physical(bdf, _driver) => {
                bind_device_to_driver(DEVICE_DRIVER_VFIO, bdf).await?
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn attach_to<V: VM>(&self, sandbox: &mut KuasarSandbox<V>) -> Result<()> {
        let id = format!("intf-{}", self.index);
        match &self.r#type {
            LinkType::Veth => {
                if let Some(intf) = &self.twin {
                    sandbox
                        .vm
                        .attach(DeviceInfo::Tap(TapDeviceInfo {
                            id,
                            index: self.index,
                            name: intf.name.to_string(),
                            mac_address: self.mac_address.to_string(),
                            fds: intf.fds.iter().map(|fd| fd.as_raw_fd()).collect(),
                        }))
                        .await?;
                } else {
                    return Err(anyhow!("no tap interface created for veth {}", self.name).into());
                }
            }
            LinkType::VhostUser(sock) => {
                sandbox
                    .vm
                    .attach(DeviceInfo::VhostUser(VhostUserDeviceInfo {
                        id,
                        socket_path: sock.to_string(),
                        mac_address: self.mac_address.to_string(),
                        r#type: "virtio-net-pci".to_string(),
                    }))
                    .await?;
            }
            LinkType::Physical(bdf, _driver) => {
                sandbox
                    .vm
                    .attach(DeviceInfo::Physical(PhysicalDeviceInfo {
                        id,
                        bdf: bdf.to_string(),
                    }))
                    .await?;
            }
            LinkType::Tap => {
                sandbox
                    .vm
                    .attach(DeviceInfo::Tap(TapDeviceInfo {
                        id,
                        index: self.index,
                        name: self.name.to_string(),
                        mac_address: self.mac_address.to_string(),
                        fds: vec![],
                    }))
                    .await?;
            }
            LinkType::Loopback => {}
            _ => {}
        }
        Ok(())
    }

    pub async fn after_detach(&mut self, _netns: &str) -> Result<()> {
        if let LinkType::Physical(bdf, driver) = &self.r#type {
            bind_device_to_driver(driver, bdf).await?
        }
        Ok(())
    }

    async fn add_qdisc_ingress(&self, netns: &str, _handle: &Handle) -> Result<()> {
        // TODO use netlink to add ingress
        let mut cmd = std::process::Command::new("tc");
        cmd.args(["qdisc", "add", "dev", &*self.name, "ingress"]);
        execute_in_netns(netns, cmd).await?;
        Ok(())
    }

    async fn add_redirect_tc_filter(&self, netns: &str, dest: &str) -> Result<()> {
        // TODO do this with netlink library
        let mut cmd = std::process::Command::new("tc");
        cmd.args([
            "filter",
            "add",
            "dev",
            &*self.name,
            "parent",
            "ffff:",
            "protocol",
            "all",
            "u32",
            "match",
            "u8",
            "0",
            "0",
            "action",
            "mirred",
            "egress",
            "redirect",
            "dev",
            dest,
        ]);
        execute_in_netns(netns, cmd).await?;
        Ok(())
    }
}

fn get_bdf_for_eth(if_name: &str) -> Result<String> {
    if if_name.len() > 16 {
        return Err(anyhow!("the interface name length is larger than 16").into());
    }
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("failed to create inet socket to get bdf: {}", e))?;
    let mut drv_info = EthtoolDrvInfo {
        cmd: ETHTOOL_GDRVINFO,
        driver: [0u8; 32],
        version: [0u8; 32],
        fw_version: [0u8; 32],
        bus_info: [0u8; 32],
        erom_version: [0u8; 32],
        reserved2: [0u8; 12],
        n_priv_flags: 0,
        n_stats: 0,
        testinfo_len: 0,
        eedump_len: 0,
        regdump_len: 0,
    };
    let mut if_name_arr = [0u8; 16];
    if_name_arr[..if_name.len()].copy_from_slice(if_name.as_bytes());
    let mut req = Ifreq {
        ifr_name: if_name_arr,
        ifr_data: &mut drv_info as *mut _ as *mut libc::c_void,
    };
    unsafe { ioctl_get_drive_info(sock, &mut req) }.map_err(|e| {
        close(sock).unwrap_or_default();
        anyhow!("failed to get drive info for {}, error: {}", if_name, e)
    })?;
    let bdf = unsafe { CStr::from_ptr(drv_info.bus_info.as_ptr() as *mut libc::c_char) };
    let bdf = bdf.to_str().map_err(|e| {
        close(sock).unwrap_or_default();
        anyhow!(
            "failed to convert the bdf of {:?} to string: {}, error: {}",
            bdf,
            if_name,
            e
        )
    })?;
    Ok(bdf.to_string())
}

async fn create_tap_in_netns(
    netns: &str,
    tap_name: &str,
    queue: u32,
    mtu: u32,
    handle: &Handle,
) -> Result<NetworkInterface> {
    let tap_name_move = tap_name.to_string();
    let fds = run_in_new_netns(netns, move || create_tap_device(&tap_name_move, queue)).await??;

    // getLinkByName
    let mut link = handle
        .link()
        .get()
        .match_name(tap_name.to_string())
        .execute();
    if let Some(msg) = link.try_next().await.map_err(|e| anyhow!("{}", e))? {
        let mut tap_intf = NetworkInterface::parse_from_message(msg, netns, queue, handle).await?;
        tap_intf.fds = fds;
        tap_intf.queue = queue;
        let link_up = handle.link().set(tap_intf.index).mtu(mtu).up().execute();
        link_up
            .await
            .map_err(|e| anyhow!("failed to set link up: {}", e))?;
        Ok(tap_intf)
    } else {
        Err(anyhow!(
            "can not get {} interface after creation in ns {}",
            tap_name,
            netns
        )
        .into())
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct ifreq {
    ifr_name: [u8; 16],
    ifr_flags: u16,
    ifr_pad: [u8; 22],
}

fn create_tap_device(tap_name: &str, mut queue: u32) -> Result<Vec<OwnedFd>> {
    if tap_name.len() > 15 {
        return Err(anyhow!("tap name {} length should less than 15", tap_name).into());
    }
    let mut if_name_arr = [0u8; 16];
    if_name_arr[..tap_name.len()].copy_from_slice(tap_name.as_bytes());

    if queue == 0 {
        queue = 1
    };

    // TODO: Remove IFF_VNET_HDR
    let flags = IFF_TAP | IFF_NO_PI | IFF_MULTI_QUEUE | IFF_VNET_HDR;

    let req = ifreq {
        ifr_name: if_name_arr,
        ifr_flags: flags as u16,
        ifr_pad: [0; 22],
    };

    let mut fds: Vec<OwnedFd> = Vec::new();
    for _i in 0..queue {
        let tun_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o0)
            .open("/dev/net/tun")
            .map_err(|e| anyhow!("failed to open tun device: {}", e))?;
        unsafe {
            ioctl_tun_set_iff(tun_file.as_raw_fd(), &req)
                .map_err(|e| anyhow!("failed to do ioctl_tun_set_iff: {}", e))?;
        }
        fds.push(OwnedFd::from(tun_file));
    }
    unsafe {
        ioctl_tun_set_persist(fds[0].as_raw_fd(), &1_u64)
            .map_err(|e| anyhow!("failed to do ioctl_tun_set_persist: {}", e))?;
    }
    Ok(fds)
}

async fn get_pci_driver(bdf: &str) -> Result<String> {
    let driver_path = format!("/sys/bus/pci/devices/{}/driver", bdf);
    let driver_dest = tokio::fs::read_link(&driver_path)
        .await
        .map_err(|e| anyhow!("fail to readlink of {} : {}", driver_path, e))?;
    let file_name = driver_dest.file_name().ok_or(anyhow!(
        "failed to get file name from driver path {:?}",
        driver_dest
    ))?;
    let file_name = file_name.to_str().ok_or(anyhow!(
        "failed to convert filename {:?} from OsStr to str",
        file_name
    ))?;
    Ok(file_name.to_string())
}

async fn bind_device_to_driver(driver: &str, bdf: &str) -> Result<()> {
    // 1. Switch the device driver
    let driver_override_path = format!("/sys/bus/pci/devices/{}/driver_override", bdf);
    write_file_async(&driver_override_path, driver).await?;

    // 2. Unbind the device from its native driver
    let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", bdf);
    if Path::new(&*unbind_path).exists() {
        write_file_async(&unbind_path, bdf).await?;
    }

    // 3. Probe driver for device
    let probe_path = "/sys/bus/pci/drivers_probe";
    write_file_async(probe_path, bdf).await?;

    // 4. Check the result
    let driver_link = format!("/sys/bus/pci/devices/{}/driver", bdf);
    let driver_path = tokio::fs::read_link(&*driver_link).await?;

    let result_driver = driver_path.file_name().ok_or(anyhow!(
        "failed to get driver name from {}",
        driver_path.display()
    ))?;
    let result_driver = result_driver.to_str().ok_or(anyhow!(
        "failed to convert the driver {} to str",
        result_driver.to_string_lossy()
    ))?;
    if result_driver != driver {
        return Err(anyhow!(
            "device {} driver is {} after executing bind to {}",
            bdf,
            result_driver,
            driver
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::network::link::create_tap_device;

    #[test]
    fn add_tap_device_with_long_name() {
        let tap_name = "add_tap_device_with_long_name";
        let res = create_tap_device(tap_name, 1);
        assert!(res.is_err());
    }

    #[test]
    fn add_and_remove_tap_device() {
        let tap_name = "test-kuasar";
        let _ = create_tap_device(tap_name, 1).expect("failed to create tap dev");

        // ip tuntap list to show tap device
        let stdout = Command::new("ip")
            .args(["tuntap", "list"])
            .output()
            .expect("failed to show ip tuntap")
            .stdout;

        let stdout = std::str::from_utf8(&stdout).expect("failed to conveert stdout");
        assert!(stdout.contains(tap_name));

        // ip tuntap del to delete tap device
        Command::new("ip")
            .args(["tuntap", "del", tap_name])
            .output()
            .expect("failed to delete tap dev");
    }
}
