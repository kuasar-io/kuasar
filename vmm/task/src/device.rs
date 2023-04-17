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

use std::{collections::HashMap, fs::read_link, os::unix::prelude::FromRawFd, sync::Arc};

use containerd_shim::{other, Error, Result};
use lazy_static::lazy_static;
use log::debug;
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

lazy_static! {
    static ref DEVICE_CONVERTORS: Vec<DeviceConverter> = {
        let converters = vec![
            convert_to_scsi_device as DeviceConverter,
            convert_to_blk_device as DeviceConverter,
        ];
        converters
    };
}

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
    #[allow(dead_code)]
    pub(crate) id: u64,
    pub(crate) tx: Sender<Device>,
    pub(crate) matcher: Box<dyn DeviceMatcher>,
}

pub struct DeviceSubscription {
    #[allow(dead_code)]
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

        let (tx, rx) = channel(1);
        for device in internal.devices.values() {
            if matcher.is_match(device) {
                let _ = tx.send(device.clone()).await;
            }
        }
        internal.id_generator += 1;
        let id = internal.id_generator;
        let ss = &mut internal.subscribers;
        ss.insert(
            id,
            DeviceSubscriber {
                id,
                tx,
                matcher: Box::new(matcher),
            },
        );

        DeviceSubscription { id, rx }
    }

    #[allow(dead_code)]
    pub async fn unsubscribe(&self, id: u64) {
        let mut internal = self.internal.lock().await;
        let ss = &mut internal.subscribers;
        ss.remove(&id);
    }

    pub async fn start(&self) {
        let internal = self.internal.clone();
        // init scsi device, some device may already exist before task start
        self.init_scsi_devices().await;
        self.init_blk_devices().await;
        tokio::spawn(async move {
            let mut socket = unsafe {
                let fd = libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                    protocols::NETLINK_KOBJECT_UEVENT as libc::c_int,
                );
                TokioSocket::from_raw_fd(fd)
            };

            socket.bind(&SocketAddr::new(0, 1)).unwrap();
            loop {
                let result = socket.recv_from_full().await;
                match result {
                    Err(e) => {
                        debug!("failed to receive uevent {}", e);
                    }
                    Ok((buf, addr)) => {
                        if addr.port_number() != 0 {
                            continue;
                        }
                        if let Ok(text) = String::from_utf8(buf) {
                            let event = Uevent::new(&text);
                            let mut intern = internal.lock().await;
                            intern.handle_event(event).await;
                        } else {
                            debug!("uevent is not utf8");
                        }
                    }
                }
            }
        });
    }

    async fn init_scsi_devices(&self) {
        let mut scsi_dir_entries = if let Ok(e) = tokio::fs::read_dir(SYSFS_SCSI_DEVICE_PATH).await
        {
            e
        } else {
            debug!("no scsi driver installed");
            return;
        };
        while let Some(entry) = scsi_dir_entries.next_entry().await.unwrap() {
            // should be a symlink of the name with "0:0:0:X"
            let scsi_index = entry.file_name().to_str().unwrap().to_string();
            if scsi_index.starts_with("0:") {
                let device_key = format!("{}/{}", scsi_index, SCSI_BLOCK_SUFFIX);
                let dev_name_parent =
                    format!("{}/{}/device/block", SYSFS_SCSI_DEVICE_PATH, scsi_index);
                let mut dev_name_entries = tokio::fs::read_dir(&dev_name_parent).await.unwrap();
                let dev_name_entry = dev_name_entries.next_entry().await.unwrap().unwrap();
                let dev_name = dev_name_entry.file_name().to_str().unwrap().to_string();
                let device = Device {
                    path: format!("{}/{}", SYSTEM_DEV_PATH, dev_name),
                    addr: scsi_index.strip_prefix("0:0:").unwrap().to_string(),
                    r#type: DeviceType::Scsi,
                };
                debug!("scan add device {:?} of devpath {}", device, device_key);
                self.internal
                    .lock()
                    .await
                    .add_device(device_key.to_string(), device)
                    .await;
            }
        }
    }

    async fn init_blk_devices(&self) {
        let mut pci_dir_entries = tokio::fs::read_dir(SYSFS_BLK_DEVICE_PATH).await.unwrap();
        while let Some(entry) = pci_dir_entries.next_entry().await.unwrap() {
            // should be a symlink of the name with "0:0:0:X"
            let dev_name = entry.file_name().to_str().unwrap().to_string();
            let metadata = entry.metadata().await.unwrap();
            // only handle virtio-block devices
            // vda -> ../../devices/pci0000:00/0000:00:02.0/virtio1/block/vda/
            // vda1 -> ../../devices/pci0000:00/0000:00:02.0/virtio1/block/vda/vda1/
            // vda2 -> ../../devices/pci0000:00/0000:00:02.0/virtio1/block/vda/vda2/
            // vda3 -> ../../devices/pci0000:00/0000:00:02.0/virtio1/block/vda/vda3/
            // vdb -> ../../devices/pci0000:00/0000:00:07.0/virtio6/block/vdb/
            if dev_name.starts_with("vd") && metadata.is_symlink() {
                let real_path = read_link(entry.path()).unwrap();
                let real_path = real_path.to_str().unwrap();
                let real_path_parts: Vec<&str> = real_path.split('/').collect();
                for part in real_path_parts {
                    if part.starts_with("0000:") {
                        let device = Device {
                            path: format!("{}/{}", SYSTEM_DEV_PATH, dev_name),
                            addr: part.to_string(),
                            r#type: DeviceType::Blk,
                        };
                        debug!("scan add device {:?} of devpath {}", device, part);
                        self.internal
                            .lock()
                            .await
                            .add_device(part.to_string(), device)
                            .await;
                    }
                }
            }
        }
    }
}

impl DeviceMonitorInternal {
    pub async fn handle_event(&mut self, event: Uevent) {
        match &*event.action {
            U_EVENT_ACTION_ADD => {
                debug!("received add uevent {:?}", event);
                self.handle_add_event(event).await
            }
            U_EVENT_ACTION_REMOVE => {
                debug!("received remove uevent {:?}", event);
                self.handle_remove_event(event).await
            }
            _ => {}
        }
    }

    async fn handle_add_event(&mut self, event: Uevent) {
        for converter in DEVICE_CONVERTORS.iter() {
            if let Some(device) = converter(&event) {
                debug!("add device {:?} of devpath {}", device, event.devpath);
                self.add_device(event.devpath.clone(), device).await;
                return;
            }
        }
    }

    async fn add_device(&mut self, device_key: String, device: Device) {
        self.devices.insert(device_key, device.clone());
        let ss = &mut self.subscribers;
        for s in ss.values() {
            if s.matcher.is_match(&device) {
                let _ = s.tx.send(device).await;
                break;
            }
        }
    }

    async fn handle_remove_event(&mut self, event: Uevent) {
        self.devices.remove(&event.devpath);
    }
}

impl Uevent {
    fn new(message: &str) -> Self {
        debug!("received uevent: {}", message);
        let mut msg_iter = message.split('\0');
        let mut event = Uevent::default();

        msg_iter.next();
        for arg in msg_iter {
            let key_val: Vec<&str> = arg.splitn(2, '=').collect();
            if key_val.len() == 2 {
                match key_val[0] {
                    U_EVENT_ACTION => event.action = String::from(key_val[1]),
                    U_EVENT_DEV_NAME => event.devname = String::from(key_val[1]),
                    U_EVENT_SUB_SYSTEM => event.subsystem = String::from(key_val[1]),
                    U_EVENT_DEV_PATH => event.devpath = String::from(key_val[1]),
                    U_EVENT_SEQ_NUM => event.seqnum = String::from(key_val[1]),
                    U_EVENT_INTERFACE => event.interface = String::from(key_val[1]),
                    _ => (),
                }
            }
        }
        event
    }
}

#[allow(dead_code)]
pub const SCSI_HOST_CHANNEL: &str = "0:0:";
pub const SCSI_BLOCK_SUFFIX: &str = "block";
pub const SYSFS_SCSI_HOST_PATH: &str = "/sys/class/scsi_host";
pub const SYSFS_SCSI_DEVICE_PATH: &str = "/sys/class/scsi_device";
pub const SYSFS_BLK_DEVICE_PATH: &str = "/sys/class/block";
pub const SYSFS_PCI_BUS_RESCAN_FILE: &str = "/sys/bus/pci/rescan";
#[allow(dead_code)]
pub const SYSFS_PCI_BUS_PREFIX: &str = "/sys/bus/pci/devices";
pub const SYSTEM_DEV_PATH: &str = "/dev";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeviceType {
    #[allow(dead_code)]
    Blk,
    Scsi,
    #[allow(dead_code)]
    Ephemeral,
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
        .unwrap_or_default();
    Ok(())
}

pub fn convert_to_scsi_device(event: &Uevent) -> Option<Device> {
    let path_parts: Vec<_> = event.devpath.split('/').collect();
    let length = path_parts.len();
    if path_parts.len() > 3
        && event.subsystem == "block"
        && path_parts[length - 3].starts_with("0:0:")
        && path_parts[length - 2] == "block"
        && !event.devname.is_empty()
    {
        let scsi_addr = path_parts[length - 3]
            .strip_prefix("0:0:")
            .unwrap()
            .to_string();
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
    let path_parts: Vec<_> = event.devpath.split('/').collect();
    let length = path_parts.len();
    if path_parts.len() > 4
        && event.subsystem == "block"
        && path_parts[length - 4].starts_with("0000:")
        && path_parts[length - 3].starts_with("virtio")
        && path_parts[length - 2] == "block"
        && !event.devname.is_empty()
    {
        let mut pci_addr = path_parts[length - 4].to_string();
        if path_parts.len() > 5 && path_parts[length - 5].to_string().starts_with("0000:") {
            pci_addr = path_parts[length - 5].to_string();
        }
        Some(Device {
            path: format!("{}/{}", SYSTEM_DEV_PATH, &event.devname),
            addr: pci_addr,
            r#type: DeviceType::Blk,
        })
    } else {
        None
    }
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
    let mut entries = tokio::fs::read_dir(SYSFS_SCSI_HOST_PATH).await.unwrap();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let host = entry.file_name();
        let host_str = host.to_str().ok_or_else(|| {
            other!(
                "failed to convert directory entry to unicode for file {:?}",
                host
            )
        })?;
        let scan_path = format!("{}/{}/{}", SYSFS_SCSI_HOST_PATH, host_str, "scan");
        tokio::fs::write(&scan_path, &scan_data).await.unwrap();
    }

    Ok(())
}
