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

use async_trait::async_trait;
use containerd_sandbox::SandboxOption;
use tokio::fs::create_dir_all;
use uuid::Uuid;
use vmm_common::SHARED_DIR_SUFFIX;

use super::devices::{
    block::{VirtioBlockDevice, VIRTIO_BLK_DRIVER},
    char::CharDevice,
    console::VirtConsole,
    create_pcie_root_bus,
    rng::VirtioRngDevice,
    serial::SerialDevice,
    vhost_user_fs::{VhostUserFs, DEFAULT_MOUNT_TAG_NAME},
    DEFAULT_CONSOLE_CHARDEV_ID, DEFAULT_CONSOLE_DEVICE_ID, DEFAULT_PCIE_BUS, DEFAULT_RNG_DEVICE_ID,
    DEFAULT_SERIAL_DEVICE_ID, PCIE_ROOTPORT_CAPACITY,
};
use crate::{
    stratovirt::{
        config::{QmpSocket, StratoVirtVMConfig, MACHINE_TYPE_MICROVM},
        devices::vsock::{find_context_id, VSockDevice},
        StratoVirtVM,
    },
    utils::get_netns,
    vm::{BlockDriver, VMFactory},
};

pub struct StratoVirtVMFactory {
    default_config: StratoVirtVMConfig,
}

#[async_trait]
impl VMFactory for StratoVirtVMFactory {
    type VM = StratoVirtVM;
    type Config = StratoVirtVMConfig;

    fn new(config: Self::Config) -> Self {
        Self {
            default_config: config,
        }
    }

    async fn create_vm(
        &self,
        id: &str,
        s: &SandboxOption,
    ) -> containerd_sandbox::error::Result<Self::VM> {
        let netns = get_netns(&s.sandbox);
        let mut vm = StratoVirtVM::new(id, &netns, &s.base_dir);
        vm.config = self.default_config.to_stratovirt_config().await?;
        vm.config.uuid = Uuid::new_v4().to_string();
        vm.config.name = format!("sandbox-{}", id);
        vm.config.pid_file = format!("{}/sandbox-{}.pid", s.base_dir, id);
        vm.block_driver = BlockDriver::from(self.default_config.block_device_driver.as_str());
        if self.default_config.common.debug {
            vm.config.log_file = Some(format!("{}/sandbox-{}.log", s.base_dir, id));
        }

        // set qmp socket
        vm.config.qmp_socket = Some(QmpSocket {
            param_key: "qmp".to_string(),
            r#type: "unix".to_string(),
            name: format!("/run/{}-qmp.sock", id),
            server: true,
            no_wait: true,
        });

        let machine_clone = vm.config.machine.r#type.clone();
        let machine_array: Vec<_> = machine_clone.split(',').collect();
        if machine_array[0] != MACHINE_TYPE_MICROVM {
            // create pcie.0 root bus object
            vm.pcie_root_bus = create_pcie_root_bus();
        }

        let transport = vm.config.machine.transport();
        // set virtio-rng device
        let rng_device = VirtioRngDevice::new(
            DEFAULT_RNG_DEVICE_ID,
            "/dev/urandom",
            transport.clone(),
            DEFAULT_PCIE_BUS,
        );
        vm.attach_to_bus(rng_device)?;

        // set console
        let serial = SerialDevice::new(
            DEFAULT_SERIAL_DEVICE_ID,
            transport.clone(),
            DEFAULT_PCIE_BUS,
        );
        vm.attach_to_bus(serial)?;
        let console_backend_chardev =
            CharDevice::new("socket", DEFAULT_CONSOLE_CHARDEV_ID, &vm.console_socket);
        vm.attach_device(console_backend_chardev);
        let virtconsole_device =
            VirtConsole::new(DEFAULT_CONSOLE_DEVICE_ID, DEFAULT_CONSOLE_CHARDEV_ID);
        vm.attach_device(virtconsole_device);

        if vm.config.kernel.image.is_some() {
            let mut image_device: VirtioBlockDevice = VirtioBlockDevice::new(
                &transport.clone().to_driver(VIRTIO_BLK_DRIVER),
                "rootfs",
                "blk-0",
                vm.config.kernel.image.clone(),
                Some(true),
            );
            image_device.bus = Some(DEFAULT_PCIE_BUS.to_string());
            vm.attach_to_bus(image_device)?;
        }

        // set vsock port as the rpc channel to agent
        let (fd, cid) = find_context_id().await?;
        let fd_index = vm.append_fd(fd);
        let vhost_vsock_device =
            VSockDevice::new(cid, transport.clone(), DEFAULT_PCIE_BUS, fd_index as i32);
        vm.attach_to_bus(vhost_vsock_device)?;
        vm.agent_socket = format!("vsock://{}:1024", cid);

        //share fs, stratovirt only support virtiofs share
        let share_fs_path = format!("{}/{}", s.base_dir, SHARED_DIR_SUFFIX);
        create_dir_all(&share_fs_path).await?;
        let absolute_virtiofs_sock = format!("{}/virtiofs.sock", s.base_dir);
        let chardev_id = format!("virtio-fs-{}", id);
        let virtiofs_chardev = CharDevice::new("socket", &chardev_id, &absolute_virtiofs_sock);
        vm.attach_device(virtiofs_chardev);
        let vhost_user_fs_device = VhostUserFs::new(
            &format!("vhost-user-fs-{}", id),
            transport.clone(),
            &chardev_id,
            DEFAULT_MOUNT_TAG_NAME,
            DEFAULT_PCIE_BUS,
        );
        vm.attach_to_bus(vhost_user_fs_device)?;

        // create virtiofs daemon
        vm.create_vitiofs_daemon(
            self.default_config.virtiofsd_conf.path.as_str(),
            s.base_dir.as_str(),
            share_fs_path.as_str(),
        );
        if machine_array[0] != MACHINE_TYPE_MICROVM {
            // set pcie-root-ports for hotplugging
            vm.create_pcie_root_ports(PCIE_ROOTPORT_CAPACITY)?;
        }

        Ok(vm)
    }
}
