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
    sandbox::KuasarSandbox,
    stratovirt::{config::StratoVirtVMConfig, StratoVirtVM},
    vm::Hooks,
};

pub struct StratoVirtHooks {
    #[allow(dead_code)]
    config: StratoVirtVMConfig,
}

impl StratoVirtHooks {
    pub fn new(config: StratoVirtVMConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Hooks<StratoVirtVM> for StratoVirtHooks {
    async fn pre_start(&self, _sandbox: &mut KuasarSandbox<StratoVirtVM>) -> Result<()> {
        // TODO
        Ok(())
    }

    async fn post_start(&self, sandbox: &mut KuasarSandbox<StratoVirtVM>) -> Result<()> {
        sandbox.data.task_address = sandbox.vm.agent_socket.to_string();
        // sync clock
        sandbox.sync_clock().await;
        Ok(())
    }
}
