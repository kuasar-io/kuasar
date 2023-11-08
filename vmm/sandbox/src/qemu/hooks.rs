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
use containerd_sandbox::error::Result;

use crate::{
    qemu::{config::QemuVMConfig, QemuVM},
    sandbox::KuasarSandbox,
    utils::get_resources,
    vm::Hooks,
};

pub struct QemuHooks {
    #[allow(dead_code)]
    config: QemuVMConfig,
}

impl QemuHooks {
    pub fn new(config: QemuVMConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Hooks<QemuVM> for QemuHooks {
    async fn pre_start(&self, sandbox: &mut KuasarSandbox<QemuVM>) -> Result<()> {
        process_annotation(sandbox).await?;
        process_config(sandbox).await?;
        Ok(())
    }

    async fn post_start(&self, sandbox: &mut KuasarSandbox<QemuVM>) -> Result<()> {
        sandbox.data.task_address = sandbox.vm.agent_socket.to_string();
        // sync clock
        sandbox.sync_clock().await;
        Ok(())
    }
}

async fn process_annotation(_sandbox: &KuasarSandbox<QemuVM>) -> Result<()> {
    Ok(())
}

async fn process_config(sandbox: &mut KuasarSandbox<QemuVM>) -> Result<()> {
    if let Some(resources) = get_resources(&sandbox.data) {
        if resources.cpu_period > 0 && resources.cpu_quota > 0 {
            // get ceil of cpus if it is not integer
            let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
            sandbox.vm.config.smp.cpus = base as u32;
            sandbox.vm.config.smp.max_cpus = base as u32;
        }
        if resources.memory_limit_in_bytes > 0 {
            sandbox.vm.config.memory.size = format!(
                "{}M",
                ((resources.memory_limit_in_bytes) as u64 / bytefmt::MIB) as u32
            );
        }
        // TODO add other resource limits to vm
    }
    Ok(())
}
