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

use sandbox_derive::{CmdLineParamSet, CmdLineParams};
use serde::{Deserialize, Serialize};

use crate::vm::{
    HypervisorCommonConfig, DEFAULT_BIND_IMAGE_FALLBACK_SIZE_MB,
    DEFAULT_BLOCK_IMAGE_SIZE_OVERHEAD_PERCENT, DEFAULT_OVERLAY_IMAGE_FALLBACK_SIZE_MB,
    DEFAULT_SMALL_DIR_MAX_BYTES, DEFAULT_SMALL_DIR_MAX_FILES, VIRTIOFS, VIRTIO_BLK,
};

const DEFAULT_KERNEL_PARAMS: &str = "console=hvc0 \
root=/dev/pmem0p1 \
rootflags=data=ordered,errors=remount-ro \
ro rootfstype=ext4";

#[derive(Serialize, Deserialize, PartialEq, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerStorageBackend {
    #[default]
    Virtiofs,
    VirtioBlk,
}

impl ContainerStorageBackend {
    pub fn as_str(&self) -> &str {
        match self {
            ContainerStorageBackend::Virtiofs => VIRTIOFS,
            ContainerStorageBackend::VirtioBlk => VIRTIO_BLK,
        }
    }

    fn sharefs_type(&self) -> &str {
        match self {
            ContainerStorageBackend::Virtiofs => "virtiofs",
            ContainerStorageBackend::VirtioBlk => "none",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VirtioBlkConfig {
    /// Allow large bind-mount directories to be converted into a virtio-blk block image.
    /// Small directories are always injected via TTRPC regardless of this setting.
    /// Defaults to false; must be explicitly enabled for HostPath volumes and similar use cases.
    pub allow_large_bind_mount: bool,
    /// Enable XFS reflink CoW for block images in Shared template mode.
    /// When true, Shared mode uses XFS with reflink=1 for fast per-instance CoW cloning;
    /// if reflink is not supported on the working filesystem the sandboxer will Fatal.
    /// Exclusive mode always uses ext4 regardless of this setting.
    /// Defaults to false (ext4 everywhere).
    pub enable_reflink_cow: bool,
    pub block_image_size_overhead_percent: u32,
    pub small_dir_max_files: usize,
    pub small_dir_max_bytes: u64,
    pub overlay_image_fallback_size_mb: u64,
    pub bind_image_fallback_size_mb: u64,
    /// Use O_DIRECT when opening block image files. Requires the sandboxer
    /// working directory to reside on a filesystem that supports direct I/O
    /// (e.g. ext4, xfs). Must be set to false when the working directory is on
    /// tmpfs (e.g. /run), which does not support O_DIRECT.
    /// Defaults to true to preserve the historical hardcoded behavior.
    #[serde(default = "default_direct_io")]
    pub direct_io: bool,
}

fn default_direct_io() -> bool {
    true
}

impl Default for VirtioBlkConfig {
    fn default() -> Self {
        Self {
            allow_large_bind_mount: false,
            enable_reflink_cow: false,
            block_image_size_overhead_percent: DEFAULT_BLOCK_IMAGE_SIZE_OVERHEAD_PERCENT,
            small_dir_max_files: DEFAULT_SMALL_DIR_MAX_FILES,
            small_dir_max_bytes: DEFAULT_SMALL_DIR_MAX_BYTES,
            overlay_image_fallback_size_mb: DEFAULT_OVERLAY_IMAGE_FALLBACK_SIZE_MB,
            bind_image_fallback_size_mb: DEFAULT_BIND_IMAGE_FALLBACK_SIZE_MB,
            direct_io: true,
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct CloudHypervisorVMConfig {
    pub path: String,
    #[serde(flatten)]
    pub common: HypervisorCommonConfig,
    pub hugepages: bool,
    pub entropy_source: String,
    pub task: TaskConfig,
    #[serde(default)]
    pub virtiofsd: VirtiofsdConfig,
    #[serde(default)]
    pub virtio_blk: VirtioBlkConfig,
    #[serde(default)]
    pub container_storage_backend: ContainerStorageBackend,
}

impl Default for CloudHypervisorVMConfig {
    fn default() -> Self {
        Self {
            path: "/usr/local/bin/cloud-hypervisor".to_string(),
            common: HypervisorCommonConfig::default(),
            hugepages: false,
            entropy_source: "/dev/urandom".to_string(),
            task: TaskConfig::default(),
            virtiofsd: VirtiofsdConfig::default(),
            virtio_blk: VirtioBlkConfig::default(),
            container_storage_backend: ContainerStorageBackend::default(),
        }
    }
}

#[derive(Deserialize, Default, Clone)]
pub struct TaskConfig {
    pub debug: bool,
    pub enable_tracing: bool,
}

#[derive(CmdLineParamSet, Deserialize, Clone, Serialize)]
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

#[derive(CmdLineParamSet, Default, Clone, Serialize, Deserialize)]
pub struct CloudHypervisorConfig {
    #[param(ignore)]
    pub path: String,
    pub api_socket: String,
    pub cpus: Cpus,
    pub memory: Memory,
    pub kernel: String,
    pub cmdline: String,
    pub initramfs: Option<String>,
    pub log_file: Option<String>,
    #[param(ignore)]
    pub debug: bool,
}

#[derive(CmdLineParams, Default, Clone, Serialize, Deserialize)]
pub struct Cpus {
    pub(crate) boot: u32,
    pub(crate) max: Option<u32>,
    pub(crate) topology: Option<String>,
    #[property(generator = "crate::utils::vec_to_string")]
    pub(crate) affinity: Vec<String>,
    #[property(generator = "crate::utils::vec_to_string")]
    pub(crate) features: Vec<String>,
}

impl Cpus {
    pub fn new(boot: u32) -> Self {
        Self {
            boot,
            max: None,
            topology: None,
            affinity: vec![],
            features: vec![],
        }
    }
}

#[derive(CmdLineParams, Default, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub(crate) size: u64,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) shared: bool,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) hugepages: bool,
    #[property(key = "hugepage_size")]
    pub(crate) hugepage_size: Option<String>,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) prefault: Option<bool>,
    #[property(generator = "crate::utils::bool_to_on_off")]
    pub(crate) thp: Option<bool>,
}

impl Memory {
    pub fn new(size: u64, shared: bool, hugepages: bool) -> Self {
        Self {
            size,
            shared,
            hugepages,
            hugepage_size: None,
            prefault: None,
            thp: None,
        }
    }
}

impl CloudHypervisorConfig {
    pub fn from(vm_config: &CloudHypervisorVMConfig) -> Self {
        let cpus = Cpus::new(vm_config.common.vcpus);
        let memory = Memory::new(
            (vm_config.common.memory_in_mb as u64) * 1024 * 1024,
            vm_config.container_storage_backend == ContainerStorageBackend::Virtiofs,
            vm_config.hugepages,
        );
        let mut cmdline = format!(
            "{} task.sharefs_type={} task.container_storage_backend={} {}",
            DEFAULT_KERNEL_PARAMS,
            vm_config.container_storage_backend.sharefs_type(),
            vm_config.container_storage_backend.as_str(),
            vm_config.common.kernel_params
        );

        if vm_config.task.debug {
            cmdline.push_str(" task.log_level=debug");
        }

        cmdline.push_str(&format!(
            " task.enable_tracing={}",
            vm_config.task.enable_tracing
        ));

        Self {
            path: vm_config.path.to_string(),
            api_socket: "".to_string(),
            cpus,
            memory,
            kernel: vm_config.common.kernel_path.to_string(),
            cmdline,
            initramfs: None,
            log_file: None,
            debug: vm_config.common.debug,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        cloud_hypervisor::config::{
            CloudHypervisorConfig, CloudHypervisorVMConfig, ContainerStorageBackend, Cpus, Memory,
        },
        config::Config,
        param::ToCmdLineParams,
    };

    #[test]
    fn test_cmdline() {
        let config = CloudHypervisorConfig {
            path: "/use/local/bin/cloud-hypervisor".to_string(),
            api_socket: "/tmp/test-api.sock".to_string(),
            cpus: Cpus {
                boot: 2,
                max: None,
                topology: Some("1:1:2:2".to_string()),
                affinity: vec!["[0@[0-3],1@[4-5,8]]".to_string()],
                features: vec!["amx".to_string()],
            },
            memory: Memory {
                size: 1024 * 1024 * 1024 * 4,
                shared: false,
                hugepages: true,
                hugepage_size: Some("2M".to_string()),
                prefault: None,
                thp: None,
            },
            kernel: "/path/to/kernel".to_string(),
            cmdline: "task.container_storage_backend=virtiofs".to_string(),
            initramfs: None,
            log_file: None,
            debug: false,
        };
        let params = config.to_cmdline_params("--");
        assert_eq!(params[0], "--api-socket");
        assert_eq!(params[1], "/tmp/test-api.sock");
        assert_eq!(params[2], "--cpus");
        assert_eq!(
            params[3],
            "boot=2,topology=1:1:2:2,affinity=[0@[0-3],1@[4-5,8]],features=amx"
        );
        assert_eq!(params[4], "--memory");
        assert_eq!(
            params[5],
            "size=4294967296,shared=off,hugepages=on,hugepage_size=2M"
        );
        assert_eq!(params[6], "--kernel");
        assert_eq!(params[7], "/path/to/kernel");
        assert_eq!(params[8], "--cmdline");
        assert_eq!(params[9], "task.container_storage_backend=virtiofs");
    }

    #[test]
    fn test_toml() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/kuasar.img\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = true
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = true
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";
        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        assert_eq!(config.sandbox.enable_tracing, false);
        assert_eq!(config.hypervisor.path, "/usr/local/bin/cloud-hypervisor");
        assert_eq!(
            config.hypervisor.common.kernel_path,
            "/var/lib/kuasar/vmlinux.bin"
        );
        assert_eq!(config.hypervisor.task.debug, true);
        assert_eq!(config.hypervisor.task.enable_tracing, false);

        assert_eq!(config.hypervisor.common.vcpus, 1);
        assert!(config.hypervisor.hugepages);
        assert_eq!(config.hypervisor.virtiofsd.thread_pool_size, 4);
        assert_eq!(config.hypervisor.virtiofsd.path, "/usr/local/bin/virtiofsd");
    }

    #[test]
    fn test_default_container_storage_backend_virtiofs() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = false
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = false
enable_tracing = false
[hypervisor.virtiofsd]
path = \"\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";
        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        assert!(config.hypervisor.container_storage_backend == ContainerStorageBackend::Virtiofs);
        let chc = CloudHypervisorConfig::from(&config.hypervisor);
        assert!(
            chc.cmdline
                .contains("task.container_storage_backend=virtiofs"),
            "expected virtiofs in cmdline, got: {}",
            chc.cmdline
        );
    }

    fn base_toml(container_storage_backend: &str) -> String {
        format!(
            r#"
[sandbox]
enable_tracing = false
[hypervisor]
path = "/usr/local/bin/cloud-hypervisor"
vcpus = 1
memory_in_mb = 1024
kernel_path = "/var/lib/kuasar/vmlinux.bin"
image_path = ""
initrd_path = ""
kernel_params = ""
hugepages = false
entropy_source = "/dev/urandom"
container_storage_backend = "{container_storage_backend}"
[hypervisor.task]
debug = false
enable_tracing = false
[hypervisor.virtiofsd]
path = ""
log_level = "info"
cache = "never"
thread_pool_size = 4
"#
        )
    }

    #[test]
    fn test_invalid_container_storage_backend_rejected() {
        let result: Result<Config<CloudHypervisorVMConfig>, _> =
            toml::from_str(&base_toml("foobar"));
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("foobar"));
    }

    #[test]
    fn test_valid_container_storage_backend_virtio_blk() {
        let config: Config<CloudHypervisorVMConfig> =
            toml::from_str(&base_toml("virtio-blk")).unwrap();
        assert!(config.hypervisor.container_storage_backend == ContainerStorageBackend::VirtioBlk);
    }

    #[test]
    fn test_valid_container_storage_backend_virtiofs() {
        let config: Config<CloudHypervisorVMConfig> =
            toml::from_str(&base_toml("virtiofs")).unwrap();
        assert!(config.hypervisor.container_storage_backend == ContainerStorageBackend::Virtiofs);
    }

    #[test]
    fn test_task_cmdline() {
        let toml_str = "
[sandbox]
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/cloud-hypervisor\"
vcpus = 1
memory_in_mb = 1024
kernel_path = \"/var/lib/kuasar/vmlinux.bin\"
image_path = \"/var/lib/kuasar/kuasar.img\"
initrd_path = \"\"
kernel_params = \"\"
hugepages = true
entropy_source = \"/dev/urandom\"
[hypervisor.task]
debug = true
enable_tracing = false
[hypervisor.virtiofsd]
path = \"/usr/local/bin/virtiofsd\"
log_level = \"info\"
cache = \"never\"
thread_pool_size = 4
";

        let config: Config<CloudHypervisorVMConfig> = toml::from_str(toml_str).unwrap();
        let chc = CloudHypervisorConfig::from(&config.hypervisor);

        assert_eq!(chc.cmdline, "console=hvc0 root=/dev/pmem0p1 rootflags=data=ordered,errors=remount-ro ro rootfstype=ext4 task.sharefs_type=virtiofs task.container_storage_backend=virtiofs  task.log_level=debug task.enable_tracing=false");
    }
}
