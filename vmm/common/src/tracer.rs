/*
Copyright 2024 The Kuasar Authors.

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

use anyhow::anyhow;
use opentelemetry::{
    global,
    sdk::{trace, trace::Tracer, Resource},
};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

pub fn setup_tracing(
    log_level: &str,
    enable_tracing: bool,
    otlp_service_name: &str,
) -> anyhow::Result<()> {
    let env_filter = init_logger_filter(log_level)
        .map_err(|e| anyhow!("failed to init logger filter: {}", e))?;

    let mut layers = vec![tracing_subscriber::fmt::layer().boxed()];
    if enable_tracing {
        let tracer = init_otlp_tracer(otlp_service_name)
            .map_err(|e| anyhow!("failed to init otlp tracer: {}", e))?;
        layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
    }

    let subscriber = Registry::default().with(env_filter).with(layers);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow!("failed to set global subscriber: {}", e))?;
    Ok(())
}

fn init_logger_filter(log_level: &str) -> anyhow::Result<EnvFilter> {
    let filter = EnvFilter::from_default_env()
        .add_directive(format!("containerd_sandbox={}", log_level).parse()?)
        .add_directive(format!("vmm_sandboxer={}", log_level).parse()?);
    Ok(filter)
}

pub fn init_otlp_tracer(otlp_service_name: &str) -> anyhow::Result<Tracer> {
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .with_trace_config(trace::config().with_resource(Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", otlp_service_name.to_string()),
        ])))
        .install_batch(opentelemetry::runtime::Tokio)?;
    Ok(tracer)
}

pub fn shutdown_tracing() {
    global::shutdown_tracer_provider();
}
