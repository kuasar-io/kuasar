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

use containerd_sandbox::error::Result;

use crate::{
    cloud_hypervisor::CloudHypervisorVM, sandbox::KuasarSandbox, utils::get_resources, vm::Hooks,
};

pub struct CloudHypervisorHooks {}

#[async_trait::async_trait]
impl Hooks<CloudHypervisorVM> for CloudHypervisorHooks {
    async fn pre_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        process_annotation(sandbox).await?;
        process_config(sandbox).await?;
        Ok(())
    }

    async fn post_start(&self, sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
        sandbox.data.task_address = sandbox.vm.agent_socket.to_string();
        // sync clock
        sandbox.sync_clock().await;
        Ok(())
    }
}

async fn process_annotation(_sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
    Ok(())
}

async fn process_config(sandbox: &mut KuasarSandbox<CloudHypervisorVM>) -> Result<()> {
    if let Some(resources) = get_resources(&sandbox.data) {
        if resources.cpu_period > 0 && resources.cpu_quota > 0 {
            // get ceil of cpus if it is not integer
            let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
            sandbox.vm.config.cpus.boot = base as u32;
            sandbox.vm.config.cpus.max = Some(base as u32);
        }
        if resources.memory_limit_in_bytes > 0 {
            sandbox.vm.config.memory.size = resources.memory_limit_in_bytes as u64;
        }
        // TODO add other resource limits to vm
    }
    Ok(())
}
