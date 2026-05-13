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

use std::{collections::HashMap, os::unix::prelude::FromRawFd, sync::Arc};

use containerd_shim::{other, Error, Result};
use log::{debug, warn};
use netlink_sys::{protocols, SocketAddr, TokioSocket};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender},
    Mutex,
};

pub const U_EVENT_ACTION: &str = "ACTION";
pub const U_EVENT_ACTION_ADD: &str = "add";
pub const U_EVENT_ACTION_REMOVE: &str = "remove";
pub const U_EVENT_DEV_PATH: &str = "DEVPATH";
pub const U_EVENT_SUB_SYSTEM: &str = "SUBSYSTEM";
pub const U_EVENT_SEQ_NUM: &str = "SEQNUM";
pub const U_EVENT_DEV_NAME: &str = "DEVNAME";
pub const U_EVENT_INTERFACE: &str = "INTERFACE";

const DEVICE_CONVERTORS: [DeviceConverter; 2] = [convert_to_scsi_device, convert_to_blk_device];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub devpath: String,
    pub devname: String,
    pub subsystem: String,
    seqnum: String,
    pub interface: String,
}

pub trait DeviceMatcher: Sync + Send + 'static {
    fn is_match(&self, device: &Device) -> bool;
}

pub type DeviceConverter = fn(&Uevent) -> Option<Device>;

pub struct DeviceSubscriber {
    pub(crate) tx: Sender<Device>,
    pub(crate) matcher: Box<dyn DeviceMatcher>,
}

pub struct DeviceSubscription {
    pub(crate) id: u64,
    pub(crate) rx: Receiver<Device>,
}

pub struct DeviceMonitor {
    internal: Arc<Mutex<DeviceMonitorInternal>>,
}

#[derive(Default)]
pub struct DeviceMonitorInternal {
    subscribers: HashMap<u64, DeviceSubscriber>,
    devices: HashMap<String, Device>,
    id_generator: u64,
}

impl DeviceMonitor {
    pub fn new() -> Self {
        Self {
            internal: Arc::new(Default::default()),
        }
    }

    pub async fn subscribe(&self, matcher: impl DeviceMatcher) -> DeviceSubscription {
        let mut internal = self.internal.lock().await;

        let (tx, rx) = channel(32);
        for device in internal.devices.values() {
            if matcher.is_match(device) {
                if let Err(e) = tx.try_send(device.clone()) {
                    warn!("failed to send existing device to subscriber: {}", e);
                }
            }
        }
        internal.id_generator += 1;
        let id = internal.id_generator;
        let ss = &mut internal.subscribers;
        ss.insert(
            id,
            DeviceSubscriber {
                tx,
                matcher: Box::new(matcher),
            },
        );

        DeviceSubscription { id, rx }
    }

    pub async fn unsubscribe(&self, id: u64) {
        let mut internal = self.internal.lock().await;
        let ss = &mut internal.subscribers;
        ss.remove(&id);
    }

    pub async fn start(&self) -> Result<()> {
        let internal = self.internal.clone();

        let mut socket = unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                protocols::NETLINK_KOBJECT_UEVENT as libc::c_int,
            );
            if fd < 0 {
                return Err(other!(
                    "failed to create netlink socket: {}",
                    std::io::Error::last_os_error()
                ));
            }
            TokioSocket::from_raw_fd(fd)
        };

        socket
            .bind(&SocketAddr::new(0, 1))
            .map_err(|e| other!("failed to bind netlink socket: {}", e))?;

        tokio::spawn(async move {
            loop {
                match socket.recv_from_full().await {
                    Ok((buf, addr)) => {
                        if addr.port_number() != 0 {
                            continue;
                        }
                        let msg = String::from_utf8_lossy(&buf);
                        let event = Uevent::new(&msg);
                        let mut intern = internal.lock().await;
                        intern.handle_event(event);
                    }
                    Err(e) => {
                        debug!("failed to receive uevent: {}", e);
                        // Exit on fatal errors like EBADF
                        if let Some(os_err) = e.raw_os_error() {
                            if os_err == libc::EBADF {
                                break;
                            }
                        }
                        // Avoid busy loop/log spam on persistent transient errors
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });

        // Initial scanning after listener is bound to avoid missing uevents
        if let Err(e) = self.init_scsi_devices().await {
            debug!("failed to init scsi devices: {}", e);
        }
        if let Err(e) = self.init_blk_devices().await {
            debug!("failed to init blk devices: {}", e);
        }
        Ok(())
    }

    async fn init_scsi_devices(&self) -> Result<()> {
        let mut scsi_dir_entries =
            tokio::fs::read_dir(SYSFS_SCSI_DEVICE_PATH)
                .await
                .map_err(|e| {
                    other!(
                        "failed to read scsi devices dir {}: {}",
                        SYSFS_SCSI_DEVICE_PATH,
                        e
                    )
                })?;
        let mut scsi_devices = Vec::new();
        loop {
            match scsi_dir_entries.next_entry().await {
                Ok(Some(scsi_entry)) => {
                    let scsi_name = scsi_entry.file_name();
                    let scsi_name_str = scsi_name.to_str().unwrap_or_default();
                    if !scsi_name_str.starts_with(SCSI_DEVICE_PREFIX) {
                        continue;
                    }

                    let scsi_block_path =
                        format!("{}/{}/device/block", SYSFS_SCSI_DEVICE_PATH, scsi_name_str);
                    let mut scsi_block_entries = match tokio::fs::read_dir(&scsi_block_path).await {
                        Ok(entries) => entries,
                        Err(e) => {
                            debug!(
                                "failed to read scsi block devices dir {}: {}",
                                scsi_block_path, e
                            );
                            continue;
                        }
                    };
                    loop {
                        match scsi_block_entries.next_entry().await {
                            Ok(Some(dev_name_entry)) => {
                                let dev_name = dev_name_entry.file_name();
                                let dev_name_str = dev_name.to_str().unwrap_or_default();
                                let devpath =
                                    match canonical_sysfs_devpath(&dev_name_entry.path()).await {
                                        Ok(path) => path,
                                        Err(e) => {
                                            debug!(
                                                "failed to get canonical devpath for {}: {}",
                                                dev_name_str, e
                                            );
                                            continue;
                                        }
                                    };

                                let addr = scsi_name_str.strip_prefix(SCSI_DEVICE_PREFIX).unwrap();
                                scsi_devices.push((
                                    devpath,
                                    Device {
                                        path: format!("/dev/{}", dev_name_str),
                                        addr: addr.to_string(),
                                        r#type: DeviceType::Scsi,
                                    },
                                ));
                            }
                            Ok(None) => break,
                            Err(e) => {
                                debug!(
                                    "failed to read next entry in scsi block devices dir: {}",
                                    e
                                );
                                break;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    debug!("failed to read next entry in scsi devices dir: {}", e);
                    break;
                }
            }
        }
        let mut internal = self.internal.lock().await;
        for (devpath, dev) in scsi_devices {
            internal.add_device(devpath, dev);
        }
        Ok(())
    }

    async fn init_blk_devices(&self) -> Result<()> {
        let mut blk_dir_entries = tokio::fs::read_dir(SYSFS_BLOCK_PATH).await.map_err(|e| {
            other!(
                "failed to read block devices dir {}: {}",
                SYSFS_BLOCK_PATH,
                e
            )
        })?;
        let mut blk_devices = Vec::new();
        loop {
            match blk_dir_entries.next_entry().await {
                Ok(Some(entry)) => {
                    let dev_name = entry.file_name();
                    let dev_name_str = dev_name.to_str().unwrap_or_default();
                    if !dev_name_str.starts_with(VIRTIO_BLK_PREFIX) {
                        continue;
                    }

                    let devpath = match canonical_sysfs_devpath(&entry.path()).await {
                        Ok(path) => path,
                        Err(e) => {
                            debug!(
                                "failed to get canonical devpath for {}: {}",
                                dev_name_str, e
                            );
                            continue;
                        }
                    };

                    if let Some(dev) = convert_blk_device(&devpath, dev_name_str) {
                        blk_devices.push((devpath, dev));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    debug!("failed to read next entry in block devices dir: {}", e);
                    break;
                }
            }
        }
        let mut internal = self.internal.lock().await;
        for (devpath, dev) in blk_devices {
            internal.add_device(devpath, dev);
        }
        Ok(())
    }
}

impl DeviceMonitorInternal {
    pub fn handle_event(&mut self, event: Uevent) {
        if event.devpath.is_empty() {
            warn!("ignoring uevent with empty devpath: {:?}", event);
            return;
        }
        match event.action.as_str() {
            U_EVENT_ACTION_ADD => {
                debug!("received add uevent {:?}", event);
                for converter in DEVICE_CONVERTORS.iter() {
                    if let Some(device) = converter(&event) {
                        debug!("add device {:?} of devpath {}", device, event.devpath);
                        self.add_device(event.devpath.clone(), device);
                        return;
                    }
                }
            }
            U_EVENT_ACTION_REMOVE => {
                debug!("received remove uevent {:?}", event);
                self.devices.remove(&event.devpath);
            }
            _ => {}
        }
    }

    fn add_device(&mut self, device_key: String, device: Device) {
        self.devices.insert(device_key, device.clone());
        for s in self.subscribers.values() {
            if s.matcher.is_match(&device) {
                if let Err(e) = s.tx.try_send(device.clone()) {
                    warn!("failed to send device to subscriber: {}", e);
                }
            }
        }
    }
}

impl Uevent {
    fn new(message: &str) -> Self {
        debug!("received uevent: {}", message);
        let mut msg_iter = message.split('\0');
        let mut event = Uevent::default();

        msg_iter.next();
        for arg in msg_iter {
            if let Some((key, value)) = arg.split_once('=') {
                match key {
                    U_EVENT_ACTION => event.action = value.to_string(),
                    U_EVENT_DEV_NAME => event.devname = value.to_string(),
                    U_EVENT_SUB_SYSTEM => event.subsystem = value.to_string(),
                    U_EVENT_DEV_PATH => event.devpath = value.to_string(),
                    U_EVENT_SEQ_NUM => event.seqnum = value.to_string(),
                    U_EVENT_INTERFACE => event.interface = value.to_string(),
                    _ => (),
                }
            }
        }
        event
    }
}

pub const SYSFS_SCSI_HOST_PATH: &str = "/sys/class/scsi_host";
pub const SYSFS_SCSI_DEVICE_PATH: &str = "/sys/class/scsi_device";
pub const SYSFS_BLOCK_PATH: &str = "/sys/class/block";
pub const SYSFS_PCI_BUS_RESCAN_FILE: &str = "/sys/bus/pci/rescan";
pub const SYSTEM_DEV_PATH: &str = "/dev";
pub const SCSI_DEVICE_PREFIX: &str = "0:0:";
pub const VIRTIO_BLK_PREFIX: &str = "vd";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeviceType {
    Blk,
    Scsi,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub(crate) path: String,
    pub(crate) addr: String,
    pub(crate) r#type: DeviceType,
}

pub async fn rescan_pci_bus() -> Result<()> {
    tokio::fs::write(SYSFS_PCI_BUS_RESCAN_FILE, "1")
        .await
        .map_err(|e| other!("failed to rescan pci bus: {}", e))?;
    Ok(())
}

fn is_pci_bdf(part: &str) -> bool {
    let parts: Vec<&str> = part.split([':', '.']).collect();
    if parts.len() != 3 && parts.len() != 4 {
        return false;
    }
    parts
        .iter()
        .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()))
}

async fn canonical_sysfs_devpath(path: &std::path::Path) -> Result<String> {
    let abs_path = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| other!("failed to canonicalize sysfs devpath {:?}: {}", path, e))?;
    let abs_path_str = abs_path
        .to_str()
        .ok_or_else(|| other!("non-unicode path"))?;
    if let Some(stripped) = abs_path_str.strip_prefix("/sys") {
        Ok(stripped.to_string())
    } else {
        Err(other!("sysfs path {:?} does not start with /sys", abs_path))
    }
}

fn convert_blk_device(devpath: &str, devname: &str) -> Option<Device> {
    if devpath.is_empty() || devname.is_empty() {
        return None;
    }
    let parts: Vec<&str> = devpath.split('/').collect();
    let len = parts.len();
    if len >= 4
        && parts[len - 3].starts_with("virtio")
        && parts[len - 2] == "block"
        && parts[len - 1] == devname
    {
        let direct_bdf = parts[len - 4];
        if is_pci_bdf(direct_bdf) {
            let mut addr = direct_bdf.to_string();
            if len >= 5 && is_pci_bdf(parts[len - 5]) {
                addr = parts[len - 5].to_string();
            }
            return Some(Device {
                path: format!("{}/{}", SYSTEM_DEV_PATH, devname),
                addr,
                r#type: DeviceType::Blk,
            });
        }
    }
    None
}

pub fn convert_to_scsi_device(event: &Uevent) -> Option<Device> {
    let path_parts: Vec<_> = event.devpath.split('/').collect();
    let length = path_parts.len();
    if length > 3
        && event.subsystem == "block"
        && path_parts[length - 3].starts_with("0:0:")
        && path_parts[length - 2] == "block"
        && !event.devname.is_empty()
    {
        let scsi_addr = path_parts[length - 3].strip_prefix("0:0:")?.to_string();
        Some(Device {
            path: format!("{}/{}", SYSTEM_DEV_PATH, &event.devname),
            addr: scsi_addr,
            r#type: DeviceType::Scsi,
        })
    } else {
        None
    }
}

pub fn convert_to_blk_device(event: &Uevent) -> Option<Device> {
    if event.subsystem != "block" {
        return None;
    }
    convert_blk_device(&event.devpath, &event.devname)
}

pub async fn scan_scsi_bus(scsi_addr: &str) -> containerd_shim::Result<()> {
    let tokens: Vec<&str> = scsi_addr.split(':').collect();
    if tokens.len() != 2 {
        return Err(other!(
            "Unexpected format for SCSI Address: {}, expect SCSIID:LUA",
            scsi_addr
        ));
    }

    // Scan scsi host passing in the channel, SCSI id and LUN.
    // Channel is always 0 because we have only one SCSI controller.
    let scan_data = format!("0 {} {}", tokens[0], tokens[1]);
    let mut entries = tokio::fs::read_dir(SYSFS_SCSI_HOST_PATH)
        .await
        .map_err(|e| {
            other!(
                "failed to read scsi host dir {}: {}",
                SYSFS_SCSI_HOST_PATH,
                e
            )
        })?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let host = entry.file_name();
        let host_str = host.to_str().ok_or_else(|| {
            other!(
                "failed to convert directory entry to unicode for file {:?}",
                host
            )
        })?;
        let scan_path = format!("{}/{}/{}", SYSFS_SCSI_HOST_PATH, host_str, "scan");
        tokio::fs::write(&scan_path, &scan_data)
            .await
            .map_err(|e| other!("failed to write scan data to {}: {}", scan_path, e))?;
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uevent_new() {
        let msg = "add@/devices/pci0000:00/0000:00:02.0/virtio1/block/vda\0\
                   ACTION=add\0\
                   DEVPATH=/devices/pci0000:00/0000:00:02.0/virtio1/block/vda\0\
                   SUBSYSTEM=block\0\
                   DEVNAME=vda\0\
                   SEQNUM=1234\0";
        let event = Uevent::new(msg);
        assert_eq!(event.action, "add");
        assert_eq!(
            event.devpath,
            "/devices/pci0000:00/0000:00:02.0/virtio1/block/vda"
        );
        assert_eq!(event.subsystem, "block");
        assert_eq!(event.devname, "vda");
        assert_eq!(event.seqnum, "1234");
    }

    #[test]
    fn test_convert_to_scsi_device() {
        let mut event = Uevent::default();
        event.subsystem = "block".to_string();
        event.devname = "sda".to_string();
        event.devpath =
            "/devices/pci0000:00/0000:00:03.0/virtio2/host0/target0:0:1/0:0:1:0/block/sda"
                .to_string();

        let device = convert_to_scsi_device(&event).unwrap();
        assert_eq!(device.addr, "1:0");
        assert_eq!(device.path, "/dev/sda");
        assert_eq!(device.r#type, DeviceType::Scsi);

        // Test non-matching path
        event.devpath = "/devices/pci0000:00/0000:00:03.0/virtio2/block/vdb".to_string();
        assert!(convert_to_scsi_device(&event).is_none());
    }

    #[test]
    fn test_is_pci_bdf() {
        assert!(is_pci_bdf("0000:00:02.0"));
        assert!(is_pci_bdf("0000:01:00.0"));
        assert!(!is_pci_bdf("virtio1"));
        assert!(!is_pci_bdf("pci0000:00"));
        assert!(!is_pci_bdf(""));
    }

    #[test]
    fn test_convert_blk_device() {
        let cases = vec![
            (
                "Standard direct PCI attachment",
                "/devices/pci0000:00/0000:00:02.0/virtio1/block/vda",
                "vda",
                Some("0000:00:02.0"),
            ),
            (
                "Nested PCI bridge (e.g. root port)",
                "/devices/pci0000:00/0000:00:02.0/0000:01:00.0/virtio6/block/vdb",
                "vdb",
                Some("0000:00:02.0"),
            ),
            (
                "Partition device (ignored)",
                "/devices/pci0000:00/0000:00:02.0/virtio1/block/vda/vda1",
                "vda1",
                None,
            ),
            (
                "Mismatched device name",
                "/devices/pci0000:00/0000:00:02.0/virtio1/block/vda",
                "vdb",
                None,
            ),
            (
                "Non-PCI device (e.g. virtio-mmio)",
                "/devices/platform/virtio-mmio.0/virtio1/block/vda",
                "vda",
                None,
            ),
        ];

        for (name, devpath, devname, expected_addr) in cases {
            let res = convert_blk_device(devpath, devname);
            if let Some(addr) = expected_addr {
                assert!(
                    res.is_some(),
                    "Test case '{}' failed: expected Some device for {}",
                    name,
                    devpath
                );
                assert_eq!(
                    res.unwrap().addr,
                    addr,
                    "Test case '{}' failed: mismatched addr for {}",
                    name,
                    devpath
                );
            } else {
                assert!(
                    res.is_none(),
                    "Test case '{}' failed: expected None for {}",
                    name,
                    devpath
                );
            }
        }
    }
}
