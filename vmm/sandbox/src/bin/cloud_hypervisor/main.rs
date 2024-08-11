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
use tracing::{error, info_span};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use vmm_common::tracer::{create_logger_filter, create_otlp_tracer};
use vmm_sandboxer::{
    args,
    cloud_hypervisor::{factory::CloudHypervisorVMFactory, hooks::CloudHypervisorHooks},
    config::Config,
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

    let config = Config::load_config(&args.config).await.unwrap();

    // Update args log level if it not presents args but in config.
    let env_filter =
        match create_logger_filter(&args.log_level.unwrap_or(config.sandbox.log_level())) {
            Ok(filter) => filter,
            Err(e) => {
                error!("failed to init logger filter: {:?}", e);
                return;
            }
        };

    let mut layers = vec![tracing_subscriber::fmt::layer().boxed()];
    if config.sandbox.enable_tracing {
        let tracer = match create_otlp_tracer("kuasar-vmm-sandboxer-clh-tracing-service", None) {
            Ok(tracer) => tracer,
            Err(e) => {
                error!("failed to init otlp tracer: {:?}", e);
                return;
            }
        };
        layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
    }

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layers)
        .init();

    let root_span = info_span!("kuasar-vmm-sandboxer-clh-root").entered();

    let mut sandboxer: KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> =
        KuasarSandboxer::new(
            config.sandbox,
            config.hypervisor,
            CloudHypervisorHooks::default(),
        );

    // Do recovery job
    sandboxer.recover(&args.dir).await;

    // Run the sandboxer
    containerd_sandbox::run(
        "kuasar-vmm-sandboxer-clh",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();

    root_span.exit();
    global::shutdown_tracer_provider();
}
