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

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use sandbox_derive::{CmdLineParamSet, CmdLineParams};
use serde::{Deserialize, Serialize};

use crate::{
    param::ToCmdLineParams, stratovirt::virtiofs::DEFAULT_VHOST_USER_FS_BIN_PATH,
    vm::HypervisorCommonConfig,
};

#[allow(dead_code)]
pub(crate) const MACHINE_TYPE_Q35: &str = "q35";
#[allow(dead_code)]
pub(crate) const MACHINE_TYPE_PC: &str = "pc";
#[allow(dead_code)]
pub(crate) const MACHINE_TYPE_VIRT: &str = "virt";
#[allow(dead_code)]
pub(crate) const MACHINE_TYPE_PSERIES: &str = "pseries";
#[allow(dead_code)]
pub(crate) const MACHINE_TYPE_CCW_VIRTIO: &str = "s390-ccw-virtio";

const DEFAULT_STRATOVIRT_PATH: &str = "/usr/bin/stratovirt";
const DEFAULT_KERNEL_PARAMS: &str =
    "console=hvc0 console=hvc1 iommu=off debug panic=1 pcie_ports=native";

#[derive(Clone, Debug, Deserialize)]
pub struct StratoVirtVMConfig {
    pub path: String,
    pub machine_type: String,
    pub block_device_driver: String,
    #[serde(flatten)]
    pub common: HypervisorCommonConfig,
    pub virtiofsd_conf: VirtiofsdConfig,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct VirtiofsdConfig {
    pub path: String,
}

impl Default for StratoVirtVMConfig {
    fn default() -> Self {
        Self {
            common: Default::default(),
            path: "stratovirt".to_string(),
            machine_type: "virt".to_string(),
            virtiofsd_conf: VirtiofsdConfig {
                path: DEFAULT_VHOST_USER_FS_BIN_PATH.to_string(),
            },
            block_device_driver: "virtio-blk".to_string(),
        }
    }
}

impl StratoVirtVMConfig {
    pub async fn to_stratovirt_config(&self) -> Result<StratoVirtConfig> {
        let mut result = StratoVirtConfig::default();
        if !self.path.is_empty() {
            result.path = self.path.to_string();
        } else {
            result.path = DEFAULT_STRATOVIRT_PATH.to_string();
        }

        result.machine = Machine {
            r#type: self.machine_type.to_string(),
            options: None,
        };

        result.smp = SMP {
            cpus: self.common.vcpus,
        };

        result.memory = Memory {
            size: format!("{}M", self.common.memory_in_mb),
        };

        result.kernel = Kernel {
            path: self.common.kernel_path.to_string(),
            image: None,
            initrd: None,
            kernel_params: "".to_string(),
        };

        if !self.common.image_path.is_empty() && !self.common.initrd_path.is_empty() {
            return Err(anyhow!("both image and initrd defined in config is not supported").into());
        }

        if self.common.image_path.is_empty() && self.common.initrd_path.is_empty() {
            return Err(
                anyhow!("either image or initrd defined in config is not supported").into(),
            );
        }

        if !self.common.image_path.is_empty() {
            result.kernel.image = Some(self.common.image_path.to_string());
        }

        if !self.common.initrd_path.is_empty() {
            result.kernel.initrd = Some(self.common.initrd_path.to_string());
        }

        if self.common.kernel_params.is_empty() {
            result.kernel.kernel_params = DEFAULT_KERNEL_PARAMS.to_string();
        } else {
            result.kernel.kernel_params =
                format!("{} {}", DEFAULT_KERNEL_PARAMS, self.common.kernel_params);
        }

        if !self.common.image_path.is_empty() {
            result
                .kernel
                .kernel_params
                .push_str(" root=/dev/vda1 ro rootfstype=ext4");
        }

        result.global_params = vec![Global {
            param: "pcie-root-port.fast-unplug=1".to_string(),
        }];

        result.knobs = Knobs {
            daemonize: true,
            disable_seccomp: true,
            prealloc: self.common.enable_mem_prealloc,
        };

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

#[derive(CmdLineParamSet, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Kernel {
    #[param(key = "kernel")]
    pub path: String,
    pub initrd: Option<String>,
    #[param(ignore)]
    pub image: Option<String>,
    #[param(key = "append")]
    pub kernel_params: String,
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
            format!("{}:{},server,nowait", self.r#type, self.name,),
        ]
    }
}

#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Global {
    #[property(ignore_key)]
    pub param: String,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(CmdLineParams, Debug, Clone, Default, Serialize, Deserialize)]
pub struct SMP {
    pub cpus: u32,
}

#[derive(CmdLineParams, Debug, Default, Clone, Serialize, Deserialize)]
#[params("m")]
pub struct Memory {
    #[property(ignore_key)]
    pub size: String,
}

#[derive(CmdLineParamSet, Debug, Clone, Default, Serialize, Deserialize)]
pub struct Knobs {
    pub daemonize: bool,
    #[param(key = "disable-seccomp")]
    pub disable_seccomp: bool,
    #[param(key = "mem-prealloc")]
    pub prealloc: bool,
}

#[derive(CmdLineParamSet, Default, Clone, Serialize, Deserialize)]
pub struct StratoVirtConfig {
    pub name: String,
    pub uuid: String,
    #[param(ignore)]
    pub path: String,
    pub machine: Machine,
    pub qmp_socket: Option<QmpSocket>,
    pub kernel: Kernel,
    pub smp: SMP,
    pub memory: Memory,
    #[param(key = "pidfile")]
    pub pid_file: String,
    #[param(key = "D")]
    pub log_file: Option<String>,
    #[param(key = "global")]
    pub global_params: Vec<Global>,
    pub knobs: Knobs,
}

#[cfg(test)]
mod tests {
    use crate::{
        param::ToCmdLineParams,
        stratovirt::config::{QmpSocket, StratoVirtVMConfig},
    };

    #[tokio::test]
    async fn test_stratovirt_params() {
        let mut vmconfig = StratoVirtVMConfig::default();

        vmconfig.common.initrd_path = "/var/lib/kuasar/initrd".to_string();

        let mut stratovirt_config = vmconfig.to_stratovirt_config().await.unwrap();
        stratovirt_config.pid_file = "/path/to/pid".to_string();
        stratovirt_config.qmp_socket = Some(QmpSocket {
            param_key: "qmp".to_string(),
            r#type: "unix".to_string(),
            name: "/path/to/qmp.sock".to_string(),
            server: true,
            no_wait: true,
        });

        stratovirt_config.uuid = "6e7b0e90-3b2e-4179-bc30-43d2b5b5964f".to_string();

        stratovirt_config.knobs.daemonize = true;
        stratovirt_config.knobs.disable_seccomp = true;
        stratovirt_config.knobs.prealloc = true;

        stratovirt_config.name = "sandbox-1".to_string();
        stratovirt_config.log_file = Some("/path/to/log".to_string());
        let params = stratovirt_config.to_cmdline_params("-");

        println!("params: {:?}", params);
        let expected_params = vec![
            "-name",
            "sandbox-1",
            "-uuid",
            "6e7b0e90-3b2e-4179-bc30-43d2b5b5964f",
            "-machine",
            "virt",
            "-qmp",
            "unix:/path/to/qmp.sock,server,nowait",
            "-kernel",
            "/var/lib/kuasar/vmlinux.bin",
            "-initrd",
            "/var/lib/kuasar/initrd",
            "-append",
            "console=hvc0 console=hvc1 iommu=off debug panic=1 pcie_ports=native",
            "-smp",
            "cpus=1",
            "-m",
            "1024M",
            "-pidfile",
            "/path/to/pid",
            "-D",
            "/path/to/log",
            "-global",
            "pcie-root-port.fast-unplug=1",
            "-daemonize",
            "-disable-seccomp",
            "-mem-prealloc",
        ];
        let expected_params_into_string: Vec<String> =
            expected_params.iter().map(|&s| s.to_string()).collect();
        assert_eq!(expected_params_into_string, params);
    }

    #[tokio::test]
    async fn test_stratovirt_params_with_image() {
        let mut vmconfig = StratoVirtVMConfig::default();
        vmconfig.common.image_path = "/var/lib/kuasar/image".to_string();
        let mut stratovirt_config = vmconfig.to_stratovirt_config().await.unwrap();
        stratovirt_config.pid_file = "/path/to/pid".to_string();
        stratovirt_config.qmp_socket = Some(QmpSocket {
            param_key: "qmp".to_string(),
            r#type: "unix".to_string(),
            name: "/path/to/qmp.sock".to_string(),
            server: true,
            no_wait: true,
        });

        stratovirt_config.uuid = "6e7b0e90-3b2e-4179-bc30-43d2b5b5964f".to_string();
        stratovirt_config.knobs.daemonize = true;
        stratovirt_config.knobs.disable_seccomp = true;
        stratovirt_config.name = "sandbox-1".to_string();
        stratovirt_config.log_file = Some("/path/to/log".to_string());
        let params = stratovirt_config.to_cmdline_params("-");

        println!("params: {:?}", params);
        let expected_params = vec![
            "-name",
            "sandbox-1",
            "-uuid",
            "6e7b0e90-3b2e-4179-bc30-43d2b5b5964f",
            "-machine",
            "virt",
            "-qmp",
            "unix:/path/to/qmp.sock,server,nowait",
            "-kernel",
            "/var/lib/kuasar/vmlinux.bin",
            "-append",
            "console=hvc0 console=hvc1 iommu=off debug panic=1 pcie_ports=native root=/dev/vda1 ro rootfstype=ext4",
            "-smp",
            "cpus=1",
            "-m",
            "1024M",
            "-pidfile",
            "/path/to/pid",
            "-D",
            "/path/to/log",
            "-global",
            "pcie-root-port.fast-unplug=1",
            "-daemonize",
            "-disable-seccomp",
        ];
        let expected_params_into_string: Vec<String> =
            expected_params.iter().map(|&s| s.to_string()).collect();
        assert_eq!(expected_params_into_string, params);
    }
}
