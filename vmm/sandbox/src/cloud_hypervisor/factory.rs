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
use containerd_sandbox::SandboxOption;

use crate::{
    cloud_hypervisor::{
        config::{CloudHypervisorVMConfig, ContainerStorageBackend},
        devices::{console::Console, fs::Fs, pmem::Pmem, rng::Rng, vsock::Vsock},
        CloudHypervisorVM,
    },
    utils::get_netns,
    vm::VMFactory,
};

pub struct CloudHypervisorVMFactory {
    vm_config: CloudHypervisorVMConfig,
}

#[cfg(test)]
mod tests {
    use containerd_sandbox::{data::SandboxData, SandboxOption};

    use super::CloudHypervisorVMFactory;
    use crate::{
        cloud_hypervisor::config::{CloudHypervisorVMConfig, ContainerStorageBackend},
        vm::VMFactory,
    };

    #[tokio::test]
    async fn test_create_vm_virtiofs_with_empty_virtiofsd_path_fails() {
        let mut config = CloudHypervisorVMConfig::default();
        config.container_storage_backend = ContainerStorageBackend::Virtiofs;
        config.virtiofsd.path = String::new();

        let factory = CloudHypervisorVMFactory::new(config);
        let s = SandboxOption {
            base_dir: "/tmp/test-sandbox".to_string(),
            sandbox: SandboxData::default(),
        };

        let err = factory.create_vm("test-id", &s).await.err().unwrap();
        assert!(
            err.to_string().contains("virtiofsd.path is not configured"),
            "expected virtiofsd.path error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_create_vm_virtio_blk_with_empty_virtiofsd_path_passes_validation() {
        let mut config = CloudHypervisorVMConfig::default();
        config.container_storage_backend = ContainerStorageBackend::VirtioBlk;
        config.virtiofsd.path = String::new();
        config.common.image_path = String::new();

        let factory = CloudHypervisorVMFactory::new(config);
        let s = SandboxOption {
            base_dir: "/tmp/test-sandbox".to_string(),
            sandbox: SandboxData::default(),
        };

        let result = factory.create_vm("test-id", &s).await;
        if let Err(ref e) = result {
            assert!(
                !e.to_string().contains("virtiofsd"),
                "virtio-blk mode must not fail virtiofsd path check, got: {}",
                e
            );
        }
    }
}

#[async_trait::async_trait]
impl VMFactory for CloudHypervisorVMFactory {
    type VM = CloudHypervisorVM;
    type Config = CloudHypervisorVMConfig;

    fn new(config: Self::Config) -> Self {
        Self { vm_config: config }
    }

    async fn create_vm(
        &self,
        id: &str,
        s: &SandboxOption,
    ) -> containerd_sandbox::error::Result<Self::VM> {
        if self.vm_config.container_storage_backend == ContainerStorageBackend::Virtiofs
            && self.vm_config.virtiofsd.path.is_empty()
        {
            return Err(anyhow!(
                "container_storage_backend is virtiofs but virtiofsd.path is not configured"
            )
            .into());
        }

        let netns = get_netns(&s.sandbox);
        let mut vm = CloudHypervisorVM::new(id, &netns, &s.base_dir, &self.vm_config);
        // add image as a disk
        if !self.vm_config.common.image_path.is_empty() {
            let rootfs_device = Pmem::new("rootfs", &self.vm_config.common.image_path, true);
            vm.add_device(rootfs_device);
        }

        // add virtio-rng device
        if !self.vm_config.entropy_source.is_empty() {
            let rng = Rng::new("rng", &self.vm_config.entropy_source);
            vm.add_device(rng);
        }

        // add vsock device
        // set guest cid
        // cid seems not important for cloud hypervisor
        let guest_socket_path = format!("{}/task.vsock", s.base_dir);
        let vsock = Vsock::new(3, &guest_socket_path, "vsock");
        vm.add_device(vsock);
        vm.agent_socket = format!("hvsock://{}:1024", guest_socket_path);

        // add console device
        // TODO add log path parameter
        let console_path = format!("/tmp/{}-task.log", id);
        let console = Console::new(&console_path, "console");
        vm.add_device(console);

        // add virtio-fs device
        if !vm.virtiofsd_config.socket_path.is_empty() {
            let fs = Fs::new("fs", &vm.virtiofsd_config.socket_path, "kuasar");
            vm.add_device(fs);
        }

        Ok(vm)
    }
}
