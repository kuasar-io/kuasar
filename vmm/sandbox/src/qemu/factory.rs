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
use containerd_sandbox::{error::Error, SandboxOption};
use tokio::fs::create_dir_all;
use uuid::Uuid;
use vmm_common::SHARED_DIR_SUFFIX;

use crate::{
    device::Transport,
    qemu::{
        config::{QemuVMConfig, QmpSocket},
        devices::{
            block::{VirtioBlockDevice, VIRTIO_BLK_DRIVER},
            char::{CharDevice, VIRT_CONSOLE_DRIVER, VIRT_SERIAL_PORT_DRIVER},
            create_bridges,
            scsi::ScsiController,
            serial::SerialBridge,
            vhost_user::{VhostCharDevice, VhostUserType},
            virtio_9p::Virtio9PDevice,
            virtio_rng::VirtioRngDevice,
            vsock::{find_context_id, VSockDevice},
        },
        QemuVM,
    },
    utils::get_netns,
    vm::{BlockDriver, ShareFsType, VMFactory},
};

pub struct QemuVMFactory {
    default_config: QemuVMConfig,
}

#[async_trait]
impl VMFactory for QemuVMFactory {
    type VM = QemuVM;
    type Config = QemuVMConfig;

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
        let mut vm = QemuVM::new(id, &netns, &s.base_dir);
        vm.config = self.default_config.to_qemu_config().await?;
        vm.config.uuid = Uuid::new_v4().to_string();
        vm.config.name = format!("sandbox-{}", id);
        vm.config.pid_file = format!("{}/sandbox-{}.pid", s.base_dir, id);
        vm.block_driver = self.default_config.block_device_driver.clone();

        // set qmp socket
        vm.config.qmp_socket = Some(QmpSocket {
            param_key: if self.default_config.machine_type == "microvm-pci" {
                "microvm-qmp".to_string()
            } else {
                "qmp".to_string()
            },
            r#type: "unix".to_string(),
            name: format!("/run/{}-qmp.sock", id),
            server: true,
            no_wait: true,
        });

        // set pci/pcie bridge
        if self.default_config.default_bridges > 0 {
            let bridges = create_bridges(
                self.default_config.default_bridges,
                &self.default_config.machine_type,
            );
            for b in bridges {
                vm.attach_device(b);
            }
        }

        // set scsi controller
        if let BlockDriver::VirtioScsi = self.default_config.block_device_driver {
            // TODO: transport may be not PCI on other cpu arch
            let scsi_controller = ScsiController::new("scsi0", Transport::Pci);
            vm.attach_device(scsi_controller);
        }

        // set console
        let serial = SerialBridge::new("serial0", Transport::Pci);
        vm.attach_device(serial);
        let console = CharDevice::new_socket(
            "console0",
            "charconsole0",
            &vm.console_socket,
            VIRT_CONSOLE_DRIVER,
            None,
        );
        vm.attach_device(console);

        // set virtio-rng device
        if !self.default_config.entropy_source.is_empty() {
            let rng_device =
                VirtioRngDevice::new("rng0", &self.default_config.entropy_source, Transport::Pci);
            vm.attach_device(rng_device);
        }

        // set vsock or serial port as the rpc channel to agent
        if self.default_config.use_vsock {
            let (fd, cid) = find_context_id().await?;
            let fd_index = vm.append_fd(fd);
            let vsock_device = VSockDevice::new(cid, Some(fd_index as i32), Transport::Pci);
            vm.attach_device(vsock_device);
            vm.agent_socket = format!("vsock://{}:1024", cid);
        } else {
            let socket = format!("{}/agent.sock", s.base_dir);
            let agent_sock = CharDevice::new_socket(
                "channel0",
                "charch0",
                &socket,
                VIRT_SERIAL_PORT_DRIVER,
                Some("agent.channel.0".to_string()),
            );
            vm.attach_device(agent_sock);
            vm.agent_socket = socket;
        }

        // share fs
        let share_fs_path = format!("{}/{}", s.base_dir, SHARED_DIR_SUFFIX);
        create_dir_all(&*share_fs_path).await?;
        match self.default_config.share_fs {
            ShareFsType::Virtio9P => {
                let multidevs = {
                    let m = self.default_config.virtio_9p_multidevs.clone();
                    if m.is_empty() {
                        None
                    } else {
                        Some(m)
                    }
                };
                let virtio_9p = Virtio9PDevice::new(
                    "extra-9p-kuasar",
                    &share_fs_path,
                    "kuasar",
                    self.default_config.virtio_9p_direct_io,
                    multidevs,
                    Transport::Pci,
                );
                vm.attach_device(virtio_9p);
            }
            ShareFsType::VirtioFS => {
                let cfg = self.default_config.virtiofsd.clone();
                match cfg {
                    Some(value) => {
                        let mut virtiofs = value;
                        virtiofs.socket_path = format!("{}/virtiofs.sock", &s.base_dir);
                        virtiofs.shared_dir = format!("{}/{}", &s.base_dir, SHARED_DIR_SUFFIX);
                        vm.virtiofsd_config = Some(virtiofs);
                    }
                    None => {
                        return Err(Error::Unimplemented(
                            "virtiofs can not start without virtiofsd config".to_string(),
                        ));
                    }
                }
                let virtiofsd = vm.virtiofsd_config.clone().unwrap();
                if !virtiofsd.socket_path.is_empty() {
                    let virtio_fs = VhostCharDevice::new(
                        "extra-fs-kuasar",
                        VhostUserType::VhostUserChar("vhost-user-fs-pci".to_string()),
                        &virtiofsd.socket_path,
                        "",
                    );
                    vm.attach_device(virtio_fs);
                }
            }
        }
        if !self.default_config.common.image_path.is_empty() {
            if self.default_config.disable_nvdimm {
                let mut image_device = VirtioBlockDevice::new(
                    &Transport::Pci.to_driver(VIRTIO_BLK_DRIVER),
                    "image1",
                    Some(self.default_config.common.image_path.to_string()),
                    true,
                );
                image_device.format = Some("raw".to_string());
                image_device.aio = Some("threads".to_string());
                image_device.r#if = Some("none".to_string());
                vm.attach_device(image_device);
            } else {
                //TODO support nvdimm device
                return Err(Error::Unimplemented("nvdimm not implemented".to_string()));
            }
        }
        Ok(vm)
    }
}
