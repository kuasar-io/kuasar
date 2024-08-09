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

use std::{collections::HashMap, os::fd::AsRawFd, path::Path, process::exit, time::Duration};

use containerd_sandbox::{cri::api::v1::NamespaceMode, PodSandboxConfig};
use containerd_shim::{
    error::Error,
    io_error, other, other_error,
    util::{mkdir, IntoOption},
    Result,
};
use log::{debug, warn};
use nix::{
    sched::{unshare, CloneFlags},
    unistd::{fork, getpid, pause, pipe, ForkResult, Pid},
};
use tokio::fs::File;
use vmm_common::{
    mount::{mount, unmount},
    storage::{Storage, DRIVERBLKTYPE, DRIVEREPHEMERALTYPE, DRIVERSCSITYPE},
    HOSTNAME_FILENAME, IPC_NAMESPACE, KUASAR_STATE_DIR, PID_NAMESPACE, SANDBOX_NS_PATH,
    UTS_NAMESPACE,
};

use crate::{
    device::{scan_scsi_bus, Device, DeviceMatcher, DeviceMonitor, DeviceType},
    CLONE_FLAG_TABLE,
};

pub struct SandboxResources {
    storages: Vec<Storage>,
    device_monitor: DeviceMonitor,
}

impl SandboxResources {
    pub async fn new() -> Self {
        let device_monitor = DeviceMonitor::new();
        device_monitor.start().await;
        Self {
            storages: vec![],
            device_monitor,
        }
    }

    pub async fn add_storages(&mut self, container_id: &str, storages: Vec<Storage>) -> Result<()> {
        for s in storages {
            self.add_storage(container_id, s).await?;
        }
        Ok(())
    }

    pub async fn defer_storages(&mut self, container_id: &str) -> Result<()> {
        for s in &mut self.storages {
            s.defer(container_id);
        }
        self.gc_storages().await?;
        Ok(())
    }

    pub async fn add_storage(&mut self, container_id: &str, mut storage: Storage) -> Result<()> {
        for s in &mut self.storages {
            if s.host_source == storage.host_source && s.r#type == storage.r#type {
                s.refer(container_id);
                return Ok(());
            }
        }

        match &*storage.driver {
            DRIVERSCSITYPE => {
                self.handle_scsi_storage(&mut storage).await?;
            }
            DRIVEREPHEMERALTYPE => {
                mount_storage(&storage).await?;
            }
            DRIVERBLKTYPE => {
                self.handle_blk_storage(&mut storage).await?;
            }
            _ => {
                unimplemented!("storage driver not implemented {}", storage.driver)
            }
        }
        self.storages.push(storage);
        Ok(())
    }

    async fn gc_storages(&mut self) -> Result<()> {
        let mut removed = vec![];
        self.storages.retain(|x| {
            if x.ref_count() == 0 {
                removed.push(x.clone());
                false
            } else {
                true
            }
        });
        for s in removed {
            debug!("unmount storage {:?}", s);
            if let Err(_e) = unmount_storage(&s).await {
                warn!("failed to unmount storage {:?}", s);
            }
        }
        let _mounts = tokio::fs::read_to_string("/proc/mounts")
            .await
            .unwrap_or_default();
        Ok(())
    }

    async fn handle_scsi_storage(&mut self, storage: &mut Storage) -> Result<()> {
        scan_scsi_bus(&storage.source).await?;
        // Retrieve the device path from SCSI address.
        let device = self.get_device(&storage.source, DeviceType::Scsi).await?;
        let path = device.path.to_string();
        storage.source = path;

        mount_storage(storage).await?;
        Ok(())
    }

    async fn handle_blk_storage(&mut self, storage: &mut Storage) -> Result<()> {
        // Retrieve the device path from pci address.
        let device = self.get_device(&storage.source, DeviceType::Blk).await?;
        let path = device.path.to_string();
        storage.source = path;

        mount_storage(storage).await?;
        Ok(())
    }

    async fn get_device(&self, addr: &str, ty: DeviceType) -> Result<Device> {
        let mut s = self
            .device_monitor
            .subscribe(TypeAddrDeviceMatcher::new(ty, addr.to_string()))
            .await;
        let res = tokio::time::timeout(Duration::from_secs(10), s.rx.recv())
            .await
            .map_err(other_error!(
                e,
                format!("timeout waiting for device with addr {} ready", addr)
            ))?;
        self.device_monitor.unsubscribe(s.id).await;
        res.ok_or_else(|| other!("can not get device with addr {}", addr))
    }
}

async fn mount_storage(storage: &Storage) -> Result<()> {
    let src_path = Path::new(&storage.source);
    if storage.fstype == "bind" && !src_path.is_dir() {
        ensure_destination_file_exists(Path::new(&storage.mount_point)).await?
    } else {
        tokio::fs::create_dir_all(&storage.mount_point)
            .await
            .map_err(other_error!(
                e,
                format!("failed to create dir {}", storage.mount_point)
            ))?;
    }

    debug!("mounting storage {:?}", storage);
    let fstype = storage.fstype.as_str().none_if(|x| x.is_empty());
    let source = storage.source.as_str().none_if(|x| x.is_empty());
    mount(fstype, source, &storage.options, &storage.mount_point).map_err(other_error!(e, ""))?;
    Ok(())
}

async fn unmount_storage(storage: &Storage) -> Result<()> {
    let src_path = Path::new(&storage.source);
    unmount(&storage.mount_point, 0).map_err(other_error!(e, ""))?;
    if storage.fstype == "bind" && !src_path.is_dir() {
        tokio::fs::remove_file(&storage.mount_point)
            .await
            .map_err(other_error!(e, ""))?
    } else {
        tokio::fs::remove_dir(&storage.mount_point)
            .await
            .map_err(other_error!(e, ""))?
    }
    Ok(())
}

async fn ensure_destination_file_exists(path: &Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    } else if path.exists() {
        return Err(other!("{:?} exists but is not a regular file", path));
    }

    let dir = path
        .parent()
        .ok_or_else(|| other!("failed to find parent path for {:?}", path))?;

    tokio::fs::create_dir_all(dir).await.map_err(other_error!(
        e,
        format!("failed to create {}", dir.display())
    ))?;

    tokio::fs::File::create(path).await.map_err(other_error!(
        e,
        format!("failed to create file {}", path.display())
    ))?;

    Ok(())
}

pub struct TypeAddrDeviceMatcher {
    addr: String,
    r#type: DeviceType,
}

impl TypeAddrDeviceMatcher {
    pub fn new(r#type: DeviceType, addr: String) -> Self {
        Self { addr, r#type }
    }
}

impl DeviceMatcher for TypeAddrDeviceMatcher {
    fn is_match(&self, device: &Device) -> bool {
        if self.addr == device.addr && self.r#type == device.r#type {
            return true;
        }
        false
    }
}

fn convert_sysctl_to_proc_path(sysctl: &str) -> String {
    let base_path = "/proc/sys/";
    let proc_path = sysctl.replace('.', "/");
    format!("{}{}", base_path, proc_path)
}

async fn write_sysctl(sysctls: HashMap<String, String>) -> Result<()> {
    for sysctl in sysctls {
        tokio::fs::write(convert_sysctl_to_proc_path(&sysctl.0), &sysctl.1)
            .await
            .map_err(io_error!(
                e,
                "failed to set sysctl {} to {}",
                &sysctl.0,
                &sysctl.1
            ))?;
    }

    Ok(())
}

fn fork_sandbox(ns_types: Vec<String>, clone_type: CloneFlags) -> Result<()> {
    debug!("fork sandbox process {:?}, {:b}", ns_types, clone_type);
    let (r, w) = pipe().map_err(other_error!(e, "create pipe when fork sandbox error"))?;
    match unsafe { fork().map_err(other_error!(e, "failed to fork"))? } {
        ForkResult::Parent { child } => {
            debug!("forked process {} for the sandbox", child);
            drop(w);
            let mut resp = [0u8; 4];
            // just wait the pipe close, do not care the read result
            nix::unistd::read(r.as_raw_fd(), &mut resp).unwrap_or_default();
            Ok(())
        }
        ForkResult::Child => {
            drop(r);
            unshare(clone_type).unwrap();
            if !ns_types.iter().any(|n| n == PID_NAMESPACE) {
                debug!("mount namespaces in child");
                mount_ns(getpid(), &ns_types);
                exit(0);
            }
            // if we need share pid ns, we fork a pause process to act as the pid 1 of the shared pid ns
            match unsafe { fork().unwrap() } {
                ForkResult::Parent { child } => {
                    mount_ns(child, &ns_types);
                    exit(0);
                }
                ForkResult::Child => {
                    debug!("mount namespaces in grand child");
                    drop(w);
                    loop {
                        pause();
                    }
                }
            }
        }
    }
}

fn mount_ns(pid: Pid, ns_types: &Vec<String>) {
    if ns_types.iter().any(|n| n == UTS_NAMESPACE) {
        let hostname = std::fs::read_to_string(Path::new(KUASAR_STATE_DIR).join(HOSTNAME_FILENAME))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !hostname.is_empty() {
            debug!("set hostname for sandbox: {}", hostname);
            nix::unistd::sethostname(hostname).unwrap();
        }
    }
    for ns_type in ns_types {
        let sandbox_ns_path = format!("{}/{}", SANDBOX_NS_PATH, ns_type);
        let ns_path = format!("/proc/{}/ns/{}", pid, ns_type);
        debug!("mount {} to {}", ns_path, sandbox_ns_path);
        mount(
            Some("none"),
            Some(ns_path.as_str()),
            &["bind".to_string()],
            &sandbox_ns_path,
        )
        .unwrap();
    }
}

async fn setup_sandbox_ns(config: &PodSandboxConfig) -> Result<()> {
    let mut nss = vec![String::from(IPC_NAMESPACE), String::from(UTS_NAMESPACE)];

    if let Some(pid_ns_mode) = config
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|s| s.namespace_options.as_ref())
        .map(|n| n.pid())
    {
        if pid_ns_mode == NamespaceMode::Pod {
            nss.push(String::from(PID_NAMESPACE));
        }
    }

    setup_persistent_ns(nss).await?;
    Ok(())
}

async fn setup_persistent_ns(ns_types: Vec<String>) -> Result<()> {
    if ns_types.is_empty() {
        return Ok(());
    }
    mkdir(SANDBOX_NS_PATH, 0o711).await?;

    let mut clone_type = CloneFlags::empty();

    for ns_type in &ns_types {
        let sandbox_ns_path = format!("{}/{}", SANDBOX_NS_PATH, ns_type);
        File::create(&sandbox_ns_path).await.map_err(io_error!(
            e,
            "failed to create: {}",
            sandbox_ns_path
        ))?;

        clone_type |= *CLONE_FLAG_TABLE
            .get(ns_type)
            .ok_or(other!("bad ns type {}", ns_type))?;
    }

    fork_sandbox(ns_types, clone_type)?;

    Ok(())
}

pub async fn setup_sandbox(config: &PodSandboxConfig) -> Result<()> {
    // Set sysctl
    if let Some(linux) = &config.linux {
        write_sysctl(linux.clone().sysctls).await?
    }

    // Set Network NS
    setup_sandbox_ns(config).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::convert_sysctl_to_proc_path;

    #[test]
    fn test_convert_sysctl_to_proc_path() {
        assert_eq!(
            convert_sysctl_to_proc_path("kernel.version"),
            "/proc/sys/kernel/version"
        );
        assert_eq!(
            convert_sysctl_to_proc_path("net.ipv4.tcp_keepalive_time"),
            "/proc/sys/net/ipv4/tcp_keepalive_time"
        );
    }
}
