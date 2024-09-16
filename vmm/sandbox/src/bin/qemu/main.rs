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
use opentelemetry::global;
use tracing::{info, info_span};
use tracing_subscriber::{layer::SubscriberExt, Layer, Registry};
use vmm_common::tracer::{init_logger_filter, init_otlp_tracer};
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
    let config_path = std::env::var("KATA_CONFIG_PATH")
        .unwrap_or_else(|_| "/usr/share/defaults/kata-containers/configuration.toml".to_string());
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

    // Initialize log filter
    let env_filter =
        init_logger_filter(&config.sandbox.log_level()).expect("failed to init logger filter");

    let mut layers = vec![tracing_subscriber::fmt::layer().boxed()];
    if config.sandbox.enable_tracing {
        let tracer = init_otlp_tracer("kuasar-vmm-sandboxer-qemu-tracing-service")
            .expect("failed to init otlp tracer");
        layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
    }

    let subscriber = Registry::default().with(env_filter).with(layers);
    tracing::subscriber::set_global_default(subscriber).expect("unable to set global subscriber");

    let root_span = info_span!("kuasar-vmm-sandboxer-qemu-root").entered();

    let sandboxer: KuasarSandboxer<QemuVMFactory, QemuHooks> = KuasarSandboxer::new(
        config.sandbox,
        config.hypervisor.clone(),
        QemuHooks::new(config.hypervisor),
    );

    info!("Kuasar vmm sandboxer clh is started");
    // Run the sandboxer
    containerd_sandbox::run(
        "kuasar-vmm-sandboxer-qemu",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();

    info!("Kuasar vmm sandboxer clh is exited");

    root_span.exit();
    global::shutdown_tracer_provider();
}
