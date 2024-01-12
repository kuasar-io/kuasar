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

use clap::Parser;
use vmm_sandboxer::{
    args, cloud_hypervisor::init_cloud_hypervisor_sandboxer, utils::init_logger, version,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return Ok(());
    }
    // Initialize sandboxer
    let sandboxer = init_cloud_hypervisor_sandboxer(&args).await?;

    // Initialize log
    init_logger(sandboxer.log_level());

    // Run the sandboxer
    containerd_sandbox::run("kuasar-sandboxer", sandboxer)
        .await
        .unwrap();

    Ok(())
}
