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
    collections::HashMap,
    fmt::{Display, Formatter},
    os::unix::io::RawFd,
};

use containerd_sandbox::error::{Error, Result};
use lazy_static::lazy_static;
use sandbox_derive::{CmdLineParamSet, CmdLineParams};
use serde::{Deserialize, Serialize};

use crate::{
    param::ToCmdLineParams,
    utils::{bool_to_on_off, get_host_memory_in_mb},
    vm::{BlockDriver, HypervisorCommonConfig, ShareFsType},
};

pub(crate) const MACHINE_TYPE_MICROVM_PCI: &str = "microvm-pci";
pub(crate) const MACHINE_TYPE_VIRT: &str = "virt";

#[cfg(target_arch = "x86_64")]
const DEFAULT_QEMU_PATH: &str = "/usr/bin/qemu-system-x86_64";
#[cfg(target_arch = "aarch64")]
const DEFAULT_QEMU_PATH: &str = "/usr/bin/qemu-system-aarch64";

lazy_static! {
    static ref SUPPORTED_MACHINES: HashMap<String, Machine> = {
        let mut sms = HashMap::new();
        #[cfg(target_arch = "x86_64")]
        sms.insert(
            "pc".to_string(),
            Machine {
                r#type: "pc".to_string(),
                #[cfg(not(feature = "virtcca"))]
                options: Some("accel=kvm,kernel_irqchip=on".to_string()),
                #[cfg(feature = "virtcca")]
                options: Some("accel=kvm,kernel_irqchip=on,kvm-type=cvm".to_string()),
            },
        );
        #[cfg(target_arch = "aarch64")]
        sms.insert(
            "virt".to_string(),
            Machine {
                r#type: "virt".to_string(),
                #[cfg(not(feature = "virtcca"))]
                options: Some("usb=off,accel=kvm,gic-version=3".to_string()),
                #[cfg(feature = "virtcca")]
                options: Some("usb=off,accel=kvm,gic-version=3,kvm-type=cvm".to_string()),
            },
        );
        sms
    };
}

#[derive(Clone, Debug, Deserialize)]
pub struct QemuVMConfig {
    #[serde(flatten)]
    pub common: HypervisorCommonConfig,
    pub machine_accelerators: String,
    pub firmware_path: String,
    pub cpu_features: String,
    pub cpu_model: String,
    pub qemu_path: String,
    pub machine_type: String,
    pub default_bridges: u32,
    pub default_max_vcpus: u32,
    pub entropy_source: String,
    // memory related configurations
    pub mem_slots: u8,
    pub mem_offset: u32,
    pub memory_path: String,
    pub file_backend_mem_path: String,
    pub mem_prealloc: bool,
    pub hugepages: bool,
    pub enable_vhost_user_store: bool,
    pub enable_swap: bool,
    pub msize_9p: u32,
    pub virtio_9p_direct_io: bool,
    pub virtio_9p_multidevs: String,

    pub enable_iothreads: bool,
    pub block_device_driver: BlockDriver,
    pub disable_nvdimm: bool,
    pub share_fs: ShareFsType,
    pub use_vsock: bool,
    pub virtiofsd: Option<VirtiofsdConfig>,
}

impl Default for QemuVMConfig {
    fn default() -> Self {
        Self {
            common: HypervisorCommonConfig::default(),
            machine_accelerators: "".to_string(),
            firmware_path: "".to_string(),
            cpu_features: "".to_string(),
            cpu_model: "host".to_string(),
            qemu_path: "/usr/bin/qemu-kvm".to_string(),
            machine_type: "pc".to_string(),
            default_bridges: 1,
            default_max_vcpus: 0,
            entropy_source: "/dev/urandom".to_string(),
            mem_slots: 1,
            mem_offset: 0,
            memory_path: "".to_string(),
            file_backend_mem_path: "".to_string(),
            mem_prealloc: false,
            hugepages: false,
            enable_vhost_user_store: false,
            enable_swap: false,
            msize_9p: 8192,
            virtio_9p_direct_io: false,
            virtio_9p_multidevs: "".to_string(),
            enable_iothreads: false,
            block_device_driver: BlockDriver::VirtioScsi,
            disable_nvdimm: false,
            share_fs: ShareFsType::Virtio9P,
            use_vsock: false,
            virtiofsd: None,
        }
    }
}

#[derive(CmdLineParamSet, Deserialize, Debug, Clone, Serialize)]
pub struct VirtiofsdConfig {
    #[param(ignore)]
    pub path: String,
    pub log_level: String,
    pub cache: String,
    pub thread_pool_size: u32,
    #[serde(default)]
    pub socket_path: String,
    #[serde(default)]
    pub shared_dir: String,
    #[serde(default)]
    pub syslog: bool,
}

impl Default for VirtiofsdConfig {
    fn default() -> Self {
        Self {
            path: "/usr/local/bin/virtiofsd".to_string(),
            log_level: "info".to_string(),
            cache: "never".to_string(),
            thread_pool_size: 4,
            socket_path: "".to_string(),
            shared_dir: "".to_string(),
            syslog: true,
        }
    }
}

impl QemuVMConfig {
    pub async fn to_qemu_config(&self) -> Result<QemuConfig> {
        let mut result = QemuConfig::default();
        if !self.qemu_path.is_empty() {
            result.path = self.qemu_path.to_string();
        } else {
            result.path = DEFAULT_QEMU_PATH.to_string();
        }
        if !self.cpu_model.is_empty() {
            result.cpu_model = CpuModel {
                cpu_model: self.cpu_model.to_string(),
                cpu_features: None,
            };
            if !self.cpu_features.is_empty() {
                result.cpu_model.cpu_features = Some(self.cpu_features.to_string());
            }
        } else {
            return Err(Error::InvalidArgument("cpu model".to_string()));
        }
        if let Some(machine) = SUPPORTED_MACHINES.get(&self.machine_type) {
            result.machine = machine.clone();
        } else {
            return Err(Error::InvalidArgument(
                "machine_type not supported!".to_string(),
            ));
        }
        if !self.firmware_path.is_empty() {
            result.bios = Some(self.firmware_path.to_string());
        }
        result.smp = SMP {
            cpus: self.common.vcpus,
            cores: 1,
            threads: 1,
            sockets: self.default_max_vcpus,
            max_cpus: self.default_max_vcpus,
        };
        if self.enable_vhost_user_store && !self.hugepages {
            return Err(Error::InvalidArgument(
                "Vhost-user-blk/scsi is enabled without hugepages enabled".to_string(),
            ));
        }
        result.memory = Memory {
            size: format!("{}M", self.common.memory_in_mb),
            slots: self.mem_slots,
            max_mem: format!("{}M", get_host_memory_in_mb().await?),
            backend_type: MemoryBackend::Ram,
            pre_alloc: self.mem_prealloc,
            shared: self.enable_vhost_user_store,
            enable_numa: self.machine_type != MACHINE_TYPE_MICROVM_PCI,
        };

        if !self.memory_path.is_empty() {
            result.memory.backend_type = MemoryBackend::File(self.memory_path.to_string());
        } else if self.hugepages {
            result.memory.backend_type = MemoryBackend::File("/dev/hugepages".to_string());
        }
        result.kernel = Kernel {
            path: self.common.kernel_path.to_string(),
            initrd: None,
            params: None,
        };
        if !self.common.initrd_path.is_empty() {
            result.kernel.initrd = Some(self.common.initrd_path.to_string());
        }
        if !self.common.kernel_params.is_empty() {
            result.kernel.params = Some(self.common.kernel_params.to_string());
        }
        result.rtc = RTC {
            base: "utc".to_string(),
            clock: "host".to_string(),
            #[cfg(not(feature = "virtcca"))]
            drift_fix: "slew".to_string(),
        };
        result.knobs = Knobs {
            no_user_config: true,
            no_defaults: true,
            no_graphic: true,
            daemonize: true,
            mlock: MemLock {
                mem_lock: !self.enable_swap,
            },
            stopped: false,
            no_reboot: true,
        };
        result.global_params = vec![Global {
            param: "kvm-pit.lost_tick_policy=discard".to_string(),
        }];
        result.vga = "none".to_string();
        #[cfg(feature = "virtcca")]
        {
            let mut set_virtcca_object = || {
                result.object = Some(String::from(
                    "tmm-guest,id=tmm0,sve-vector-length=128,num-pmu-counters=1",
                ));
            };
            set_virtcca_object();
        }
        Ok(result)
    }
}

#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Machine {
    #[property(ignore_key)]
    pub r#type: String,
    #[property(ignore_key)]
    pub options: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QmpSocket {
    pub param_key: String,
    pub r#type: String,
    pub name: String,
    pub server: bool,
    pub no_wait: bool,
}

impl ToCmdLineParams for QmpSocket {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        vec![
            format!("{}{}", hyphen, self.param_key),
            format!(
                "{}:{},server={},wait={}",
                self.r#type,
                self.name,
                bool_to_on_off(&self.server),
                bool_to_on_off(&!self.no_wait)
            ),
        ]
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct RTC {
    pub base: String,
    pub clock: String,
    #[cfg(not(feature = "virtcca"))]
    #[property(key = "driftfix")]
    pub drift_fix: String,
}

#[derive(CmdLineParamSet, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Kernel {
    #[param(key = "kernel")]
    pub path: String,
    pub initrd: Option<String>,
    #[param(key = "append")]
    pub params: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub size: String,
    pub slots: u8,
    pub max_mem: String,
    pub backend_type: MemoryBackend,
    pub pre_alloc: bool,
    pub shared: bool,
    pub enable_numa: bool,
}

impl ToCmdLineParams for Memory {
    fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
        let mut params = vec![];
        if !self.size.is_empty() {
            params.push(format!("{}m", hyphen));
            params.push(format!(
                "{},slots={},maxmem={}",
                self.size, self.slots, self.max_mem
            ));
        }
        // -machine with memory-backend is only supported by qemu with version higher than 5.0,
        // so we return directly here if numa is not supported.
        if !self.enable_numa {
            return params;
        }

        let id = "dimm1";
        match &self.backend_type {
            MemoryBackend::Ram => {
                params.push(format!("{}object", hyphen));
                params.push(format!(
                    "memory-backend-ram,id={},size={},prealloc={},share={}",
                    id,
                    self.size,
                    bool_to_on_off(&self.pre_alloc),
                    bool_to_on_off(&self.shared)
                ));
            }
            MemoryBackend::File(f) => {
                params.push(format!("{}object", hyphen));
                params.push(format!(
                    "memory-backend-file,id={},size={},mem-path={},prealloc={},share={}",
                    id,
                    self.size,
                    f,
                    bool_to_on_off(&self.pre_alloc),
                    bool_to_on_off(&self.shared)
                ));
            }
        }
        if self.enable_numa {
            params.push(format!("{}numa", hyphen));
            params.push(format!("node,memdev={}", id));
        } else {
            params.push(format!("{}machine", hyphen));
            params.push(format!("memory-backend={}", id));
        }

        params
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryBackend {
    Ram,
    File(String),
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::Ram
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct SMP {
    pub cpus: u32,
    #[property(predicate = "self.cores > 0")]
    pub cores: u32,
    #[property(predicate = "self.threads > 0")]
    pub threads: u32,
    #[property(predicate = "self.sockets > 0")]
    pub sockets: u32,
    #[property(key = "maxcpus", predicate = "self.max_cpus > 0")]
    pub max_cpus: u32,
}

#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
#[params("cpu")]
pub struct CpuModel {
    #[property(ignore_key)]
    pub cpu_model: String,
    #[property(ignore_key)]
    pub cpu_features: Option<String>,
}

#[derive(CmdLineParamSet, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Knobs {
    pub no_user_config: bool,
    #[param(key = "nodefaults")]
    pub no_defaults: bool,
    #[param(key = "nographic")]
    pub no_graphic: bool,
    pub daemonize: bool,
    #[param(ignore)]
    pub mlock: MemLock,
    #[param(key = "S")]
    pub stopped: bool,
    pub no_reboot: bool,
}

#[derive(CmdLineParams, Debug, Clone, Serialize, Deserialize)]
#[params("incoming")]
pub struct Incoming {
    #[property(ignore_key)]
    pub migration_type: MigrationType,
}

impl Default for Incoming {
    fn default() -> Self {
        Self {
            migration_type: MigrationType::Defer,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MigrationType {
    FD(RawFd),
    Exec(String),
    Tcp(String),
    Defer,
}

impl Display for MigrationType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match &self {
            MigrationType::FD(fd) => format!("fd:{}", fd),
            MigrationType::Exec(cmd) => format!("exec:{}", cmd),
            MigrationType::Tcp(endpoint) => format!("tcp:{}", endpoint),
            MigrationType::Defer => "defer".to_string(),
        };
        write!(f, "{}", s)
    }
}

#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
#[params("overcommit")]
pub struct MemLock {
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub mem_lock: bool,
}

#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Global {
    #[property(ignore_key)]
    pub param: String,
}

#[derive(CmdLineParamSet, Debug, Default, Clone, Serialize, Deserialize)]
pub struct QemuConfig {
    pub name: String,
    pub uuid: String,
    #[param(ignore)]
    pub path: String,
    pub machine: Machine,
    pub qmp_socket: Option<QmpSocket>,
    pub cpu_model: CpuModel,
    pub rtc: RTC,
    pub vga: String,
    pub kernel: Kernel,
    pub smp: SMP,
    pub memory: Memory,
    pub global_params: Vec<Global>,
    pub knobs: Knobs,
    pub bios: Option<String>,
    pub pflash: Option<String>,
    pub incoming: Option<Incoming>,
    pub io_threads: Vec<IOThread>,
    #[param(key = "pidfile")]
    pub pid_file: String,
    #[param(key = "D")]
    pub log_file: Option<String>,
    #[cfg(feature = "virtcca")]
    #[param(key = "object")]
    pub object: Option<String>,
}

#[derive(CmdLineParamSet, Debug, Default, Clone, Serialize, Deserialize)]
pub struct IOThread {
    object: Object,
}

#[derive(CmdLineParams, Debug, Default, Clone, Serialize, Deserialize)]
pub struct Object {
    #[property(ignore_key)]
    driver: String,
    id: String,
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        param::ToCmdLineParams,
        qemu::config::{IOThread, Incoming, MigrationType, Object, QemuVMConfig, QmpSocket},
    };

    #[tokio::test]
    async fn test_qemu_params() {
        let vmconfig = QemuVMConfig::default();
        let mut config = vmconfig.to_qemu_config().await.unwrap();
        config.pid_file = "/path/to/pid".to_string();
        config.qmp_socket = Some(QmpSocket {
            param_key: "qmp".to_string(),
            r#type: "unix".to_string(),
            name: "/path/to/qmp.sock".to_string(),
            server: true,
            no_wait: true,
        });
        config.knobs.stopped = true;
        config.incoming = Some(Incoming {
            migration_type: MigrationType::Tcp("1.1.1.1:1000".to_string()),
        });
        config.io_threads = vec![IOThread {
            object: Object {
                driver: "io-thread".to_string(),
                id: "1".to_string(),
            },
        }];
        config.name = "sandbox-1".to_string();
        config.uuid = Uuid::new_v4().to_string();
        config.log_file = Some("/path/to/log".to_string());
        let params = config.to_cmdline_params("-");
        eprintln!("params: {:?}", params);
        // TODO asserts
    }
}
