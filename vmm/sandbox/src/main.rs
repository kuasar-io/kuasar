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

use std::path::Path;

use log::info;

use crate::{
    config::Config,
    kata_config::KataConfig,
    sandbox::{KuasarSandbox, KuasarSandboxer},
};

macro_rules! cfg_qemu {
    ($($item:item)*) => {
        $(
            #[cfg(feature = "qemu")]
            #[cfg_attr(docsrs, doc(cfg(feature = "qemu")))]
            $item
        )*
    }
}

macro_rules! cfg_cloud_hypervisor {
    ($($item:item)*) => {
        $(
            #[cfg(feature = "cloud_hypervisor")]
            #[cfg_attr(docsrs, doc(cfg(feature = "cloud_hypervisor")))]
            $item
        )*
    }
}

macro_rules! cfg_stratovirt {
    ($($item:item)*) => {
        $(
            #[cfg(feature = "stratovirt")]
            #[cfg_attr(docsrs, doc(cfg(feature = "stratovirt")))]
            $item
        )*
    }
}

cfg_qemu! {
    use crate::qemu::factory::QemuVMFactory;
    use crate::qemu::hooks::QemuHooks;
}

cfg_cloud_hypervisor! {
    use crate::cloud_hypervisor::config::CloudHypervisorVMConfig;
    use crate::cloud_hypervisor::factory::CloudHypervisorVMFactory;
    use crate::cloud_hypervisor::hooks::CloudHypervisorHooks;
}

cfg_stratovirt! {
    use crate::stratovirt::config::StratoVirtVMConfig;
    use crate::stratovirt::factory::StratoVirtVMFactory;
    use crate::stratovirt::hooks::StratoVirtHooks;
}

#[macro_use]
mod device;

mod container;
mod io;
mod kata_config;
mod network;
mod sandbox;
mod storage;
mod utils;
mod vm;

#[cfg(feature = "qemu")]
mod qemu;

#[cfg(feature = "stratovirt")]
mod stratovirt;

mod client;
#[cfg(feature = "cloud_hypervisor")]
mod cloud_hypervisor;
mod config;
mod param;

pub const NAMESPACE_PID: &str = "pid";
pub const NAMESPACE_NET: &str = "network";
pub const NAMESPACE_MNT: &str = "mount";
pub const NAMESPACE_CGROUP: &str = "cgroup";

pub const FS_SHARE_PATH: &str = "shared_fs";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_default_env()
        .format_timestamp_micros()
        .init();
    #[cfg(feature = "qemu")]
    let sandboxer: KuasarSandboxer<QemuVMFactory, QemuHooks> = {
        // for compatible of kata config
        let config_path = std::env::var("KATA_CONFIG_PATH").unwrap_or_else(|_| {
            "/usr/share/defaults/kata-containers/configuration.toml".to_string()
        });
        let path = Path::new(&config_path);
        if path.exists() {
            KataConfig::init(path).await.unwrap();
        }
        let vmm_config = KataConfig::hypervisor_config("qemu", |h| h.clone()).await?;
        let vmm_config = vmm_config.to_qemu_config()?;
        let sandbox_config = KataConfig::sandbox_config("qemu").await?;
        let hooks = QemuHooks::new(vmm_config.clone());
        KuasarSandboxer::new(sandbox_config, vmm_config, hooks)
    };

    #[cfg(feature = "stratovirt")]
    let sandboxer: KuasarSandboxer<StratoVirtVMFactory, StratoVirtHooks> = {
        let os_args: Vec<_> = std::env::args_os().collect();
        let mut config_path = "/var/lib/kuasar/config_stratovirt.toml".to_string();
        let mut dir = None;
        for i in 0..os_args.len() {
            if os_args[i].to_str().unwrap() == "--config" {
                config_path = os_args[i + 1].to_str().unwrap().to_string()
            }
            if os_args[i].to_str().unwrap() == "--dir" {
                let dir_path = os_args[i + 1].to_str().unwrap().to_string();
                if !Path::new(&dir_path).exists() {
                    tokio::fs::create_dir_all(&dir_path).await.unwrap();
                }
                dir = Some(dir_path);
            }
        }
        let path = Path::new(&config_path);
        let config: Config<StratoVirtVMConfig> = if path.exists() {
            Config::parse(path).await?
        } else {
            panic!("config file {} not exist", config_path);
        };
        let hooks = StratoVirtHooks::new(config.hypervisor.clone());
        KuasarSandboxer::new(config.sandbox, config.hypervisor, hooks)
    };

    #[cfg(feature = "cloud_hypervisor")]
    let sandboxer: KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> = {
        let os_args: Vec<_> = std::env::args_os().collect();

        let mut config_path = "/var/lib/kuasar/config_clh.toml".to_string();
        let mut dir = None;
        for i in 0..os_args.len() {
            if os_args[i].to_str().unwrap() == "--config" {
                config_path = os_args[i + 1].to_str().unwrap().to_string()
            }
            if os_args[i].to_str().unwrap() == "--dir" {
                let dir_path = os_args[i + 1].to_str().unwrap().to_string();
                if !Path::new(&dir_path).exists() {
                    tokio::fs::create_dir_all(&dir_path).await.unwrap();
                }
                dir = Some(dir_path);
            }
        }
        let path = Path::new(&config_path);
        let config: Config<CloudHypervisorVMConfig> = if path.exists() {
            Config::parse(path).await?
        } else {
            panic!("config file {} not exist", config_path);
        };
        let hooks = CloudHypervisorHooks {};
        let mut s = KuasarSandboxer::new(config.sandbox, config.hypervisor, hooks);
        if let Some(d) = dir {
            s.recover(&d).await.unwrap();
        }
        s
    };

    #[cfg(any(feature = "cloud_hypervisor", feature = "qemu", feature = "stratovirt"))]
    containerd_sandbox::run("kuasar-sandboxer", sandboxer)
        .await
        .unwrap();
    Ok(())
}
