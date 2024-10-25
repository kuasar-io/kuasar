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
use vmm_common::{signal, trace};
use vmm_sandboxer::{
    args,
    config::Config,
    kata_config::KataConfig,
    qemu::{factory::QemuVMFactory, hooks::QemuHooks},
    sandbox::KuasarSandboxer,
    version,
};

#[tokio::main]
async fn main() {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    // For compatibility with kata config
    let config_path = std::env::var("KATA_CONFIG_PATH").unwrap_or_default();
    let path = std::path::Path::new(&config_path);

    let config = if path.exists() {
        KataConfig::init(path).await.unwrap();
        let sandbox_config = KataConfig::sandbox_config("qemu").await.unwrap();
        let vmm_config = KataConfig::hypervisor_config("qemu", |h| h.clone())
            .await
            .unwrap()
            .to_qemu_config()
            .unwrap();
        Config::new(sandbox_config, vmm_config)
    } else {
        Config::load_config(&args.config).await.unwrap()
    };

    let log_level = config.sandbox.log_level();
    let service_name = "kuasar-vmm-sandboxer-qemu-service";
    trace::set_enabled(config.sandbox.enable_tracing);
    trace::setup_tracing(&log_level, service_name).unwrap();

    let mut sandboxer: KuasarSandboxer<QemuVMFactory, QemuHooks> = KuasarSandboxer::new(
        config.sandbox,
        config.hypervisor.clone(),
        QemuHooks::new(config.hypervisor),
    );

    tokio::spawn(async move {
        signal::handle_signals(&log_level, service_name).await;
    });

    // Do recovery job
    sandboxer.recover(&args.dir).await;

    // Run the sandboxer
    containerd_sandbox::run(
        "kuasar-vmm-sandboxer-qemu",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();
}
