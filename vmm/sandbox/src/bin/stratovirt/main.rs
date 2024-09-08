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
use tracing_subscriber::{layer::SubscriberExt, Registry};
use tracing_subscriber::Layer;
use vmm_common::tracer::{init_logger_filter, init_otlp_tracer};
use vmm_sandboxer::{
    args,
    config::Config,
    sandbox::KuasarSandboxer,
    stratovirt::{
        config::StratoVirtVMConfig, factory::StratoVirtVMFactory, hooks::StratoVirtHooks,
    },
    version,
};

#[tokio::main]
async fn main() {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    let config: Config<StratoVirtVMConfig> = Config::load_config(&args.config).await.unwrap();

    // Update args log level if it not presents args but in config.
    let env_filter = init_logger_filter(&args.log_level.unwrap_or(config.sandbox.log_level()))
        .expect("failed to init logger filter");

    let mut layers = vec![tracing_subscriber::fmt::layer().boxed()];
    if config.sandbox.enable_tracing {
        let tracer = init_otlp_tracer("kuasar-vmm-sandboxer-stratovirt-otlp-service")
            .expect("failed to init otlp tracer");
        layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
    }

    let subscriber = Registry::default().with(env_filter).with(layers);
    tracing::subscriber::set_global_default(subscriber).expect("unable to set global subscriber");

    let root_span = info_span!("kuasar-vmm-sandboxer-stratovirt-root").entered();

    let mut sandboxer: KuasarSandboxer<StratoVirtVMFactory, StratoVirtHooks> = KuasarSandboxer::new(
        config.sandbox,
        config.hypervisor.clone(),
        StratoVirtHooks::new(config.hypervisor),
    );

    // Do recovery job
    sandboxer.recover(&args.dir).await;

    info!("Kuasar vmm sandboxer stratovirt is started");
    // Run the sandboxer
    containerd_sandbox::run(
        "kuasar-vmm-sandboxer-stratovirt",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();

    info!("Kuasar vmm sandboxer stratovirt is exited");

    root_span.exit();
    global::shutdown_tracer_provider();
}
