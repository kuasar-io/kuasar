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

use std::{collections::HashMap, path::Path, str::FromStr};

use anyhow::anyhow;
use containerd_sandbox::error::{Error, Result};
use lazy_static::lazy_static;
use serde_derive::Deserialize;
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::{
    qemu::config::QemuVMConfig, sandbox::SandboxConfig, utils::read_file, vm::ShareFsType,
};

lazy_static! {
    pub static ref CONFIG: RwLock<KataConfig> = {
        let config = KataConfig {
            hypervisor: Default::default(),
            runtime: Default::default(),
        };
        RwLock::new(config)
    };
    static ref DEFAULT_KERNEL_PARAMS: Vec<(&'static str, &'static str)> = vec![
        ("tsc", "reliable"),
        ("no_timer_check", ""),
        ("rcupdate.rcu_expedited", "1"),
        ("i8042.direct", "1"),
        ("i8042.dumbkbd", "1"),
        ("i8042.nopnp", "1"),
        ("i8042.noaux", "1"),
        ("noreplace-smp", ""),
        ("reboot", "k"),
        ("console", "hvc0"),
        ("console", "hvc1"),
        ("iommu", "off"),
        ("cryptomgr.notests", ""),
        ("net.ifnames", "0"),
        ("pci", "lastbus=0"),
    ];
}

#[derive(Debug, Deserialize, Clone)]
pub struct KataConfig {
    pub hypervisor: HashMap<String, Hypervisor>,
    pub runtime: Runtime,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Runtime {
    #[serde(default)]
    pub enable_cpu_memory_hotplug: bool,
    #[serde(default)]
    pub internetworking_model: String,
    #[serde(default)]
    pub disable_guest_seccomp: bool,
    #[serde(default)]
    pub disable_hostdir_mount: bool,
    #[serde(default)]
    pub hostdir_whitelist: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Hypervisor {
    pub path: String,
    pub kernel: String,
    #[serde(default)]
    pub initrd: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub machine_accelerators: String,
    #[serde(default)]
    pub cpu_features: String,
    #[serde(default)]
    pub kernel_params: String,
    #[serde(default)]
    pub machine_type: String,
    #[serde(default)]
    pub block_device_driver: String,
    #[serde(default)]
    pub entropy_source: String,
    #[serde(default)]
    pub shared_fs: String,
    #[serde(default)]
    pub virtio_fs_daemon: String,
    #[serde(default)]
    pub virtio_fs_cache: String,
    #[serde(default)]
    pub virtio_fs_extra_args: Vec<String>,
    #[serde(default)]
    pub vhost_user_store_path: String,
    #[serde(default)]
    pub virtio_fs_cache_size: u32,
    #[serde(default)]
    pub virtio_fs_fuse_version: String,
    #[serde(default)]
    pub virtio_9p_direct_io: bool,
    #[serde(default)]
    pub virtio_9p_multidevs: String,
    #[serde(default)]
    pub block_device_cache_set: bool,
    #[serde(default)]
    pub block_device_cache_direct: bool,
    #[serde(default)]
    pub block_device_cache_noflush: bool,
    #[serde(default)]
    pub enable_vhost_user_store: bool,
    #[serde(default)]
    pub default_vcpus: i32,
    #[serde(default)]
    pub default_maxvcpus: u32,
    #[serde(default)]
    pub default_memory: u32,
    #[serde(default)]
    pub memory_slots: u32,
    #[serde(default)]
    pub memory_offset: u32,
    #[serde(default)]
    pub default_bridges: u32,
    #[serde(default)]
    pub default_root_ports: u32,
    #[serde(default)]
    pub msize_9p: u32,
    #[serde(default)]
    pub disable_block_device_use: bool,
    #[serde(default)]
    pub enable_mem_prealloc: bool,
    #[serde(default)]
    pub enable_hugepages: bool,
    #[serde(default)]
    pub enable_swap: bool,
    #[serde(default)]
    pub enable_debug: bool,
    #[serde(default)]
    pub disable_nesting_checks: bool,
    #[serde(default)]
    pub enable_iothreads: bool,
    #[serde(default)]
    pub use_vsock: bool,
    #[serde(default)]
    pub hotplug_vfio_on_root_bus: bool,
    #[serde(default)]
    pub guest_hook_path: String,
    #[serde(default)]
    pub hypervisor_params: String,
    #[serde(default)]
    pub disable_image_nvdimm: bool,
    #[serde(default)]
    pub remote_ip: String,
    #[serde(default)]
    pub min_port: u32,
    #[serde(default)]
    pub max_port: u32,
    #[serde(default)]
    pub qemu_tls: bool,
    #[serde(default)]
    pub qemu_cacert_file: String,
    #[serde(default)]
    pub qemu_cert_file: String,
    #[serde(default)]
    pub qemu_key_file: String,
    #[serde(default)]
    pub qemu_crypto_path: String,
}

impl KataConfig {
    pub async fn init(config_path: &Path) -> Result<()> {
        let toml_str = read_file(config_path).await?;
        let conf: KataConfig =
            toml::from_str(&toml_str).map_err(|e| anyhow!("failed to parse kata config {}", e))?;
        let mut static_conf = CONFIG.write().await;
        *static_conf = conf;
        Ok(())
    }

    pub async fn get() -> Result<RwLockReadGuard<'static, KataConfig>> {
        let config = CONFIG.read().await;
        Ok(config)
    }

    pub async fn hypervisor_config<T>(h: &str, extract: impl Fn(&Hypervisor) -> T) -> Result<T> {
        let config = KataConfig::get().await?;
        let h = config
            .hypervisor
            .get(h)
            .ok_or_else(|| Error::NotFound(format!("no hypervisor config of {} in kata", h)))?;
        Ok(extract(h))
    }

    pub async fn sandbox_config(h: &str) -> Result<SandboxConfig> {
        let config = KataConfig::get().await?;
        let _h = config
            .hypervisor
            .get(h)
            .ok_or_else(|| Error::NotFound(format!("no hypervisor config of {} in kata", h)))?;
        Ok(SandboxConfig::default())
    }
}

impl Hypervisor {
    #[allow(clippy::field_reassign_with_default)]
    pub fn to_qemu_config(&self) -> Result<QemuVMConfig> {
        let mut res = QemuVMConfig::default();
        res.qemu_path = self.path.to_string();
        res.machine_type = self.machine_type.to_string();
        res.default_bridges = self.default_bridges;
        res.cpu_model = match &*self.machine_type {
            "microvm-pci" => "microvm".to_string(),
            _ => "host".to_string(),
        };
        // TODO add nested run detection
        res.firmware_path = self.firmware.to_string();
        res.enable_swap = self.enable_swap;
        res.hugepages = self.enable_hugepages;
        res.enable_vhost_user_store = self.enable_vhost_user_store;
        res.mem_prealloc = self.enable_mem_prealloc;
        res.mem_slots = self.memory_slots as u8;
        res.common.memory_in_mb = self.default_memory;
        if !self.cpu_features.is_empty() {
            // kick out any space character
            res.cpu_features = self
                .cpu_features
                .split(',')
                .map(|x| x.trim().to_string())
                .collect::<Vec<_>>()
                .join(",");
        }
        res.default_max_vcpus = self.default_maxvcpus;
        res.enable_iothreads = self.enable_iothreads;
        res.machine_accelerators = self.machine_accelerators.to_string();
        if !self.entropy_source.is_empty() {
            res.entropy_source = self.entropy_source.to_string();
        }
        res.virtio_9p_direct_io = self.virtio_9p_direct_io;
        if !self.virtio_9p_multidevs.is_empty() {
            res.virtio_9p_multidevs = self.virtio_9p_multidevs.to_string();
        }
        if self.default_vcpus > 0 {
            res.common.vcpus = self.default_vcpus as u32
        }
        res.common.kernel_params = self.kernel_params.to_string();
        res.common.debug = self.enable_debug;
        res.common.kernel_path = self.kernel.to_string();
        res.common.initrd_path = self.initrd.to_string();
        res.common.image_path = self.image.to_string();
        res.disable_nvdimm = self.disable_image_nvdimm;
        if self.msize_9p > 0 {
            res.msize_9p = self.msize_9p;
        }
        res.share_fs = ShareFsType::from_str(&self.shared_fs)?;

        let kernel_params = DEFAULT_KERNEL_PARAMS
            .iter()
            .fold("".to_string(), |kvs, (k, v)| {
                let kv = if !v.is_empty() {
                    format!(" {}={}", k, v)
                } else {
                    format!(" {}", k)
                };
                format!("{}{}", kvs, kv)
            });
        res.common.kernel_params.push_str(&kernel_params);
        if !res.common.image_path.is_empty() {
            if !res.disable_nvdimm {
                res.machine_accelerators.push_str("nvdimm");
                res.common.kernel_params.push_str(" root=/dev/pmem0p1 rootflags=dax,data=ordered,errors=remount-ro ro rootfstype=ext4");
            } else {
                res.common.kernel_params.push_str(
                    " root=/dev/vda1 rootflags=data=ordered,errors=remount-ro ro rootfstype=ext4",
                );
            }
        }
        if res.common.debug {
            res.common
                .kernel_params
                .push_str(" debug agent.debug_console agent.debug_console_vport=2022");
        }
        res.use_vsock = self.use_vsock;
        Ok(res)
    }
}
