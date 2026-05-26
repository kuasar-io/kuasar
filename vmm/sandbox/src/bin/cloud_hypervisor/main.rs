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

use std::path::Path;

use clap::Parser;
use vmm_common::{signal, trace};
use vmm_sandboxer::{
    args,
    cloud_hypervisor::{
        config::{CloudHypervisorVMConfig, ContainerStorageBackend},
        factory::CloudHypervisorVMFactory,
        hooks::CloudHypervisorHooks,
    },
    config::Config,
    sandbox::KuasarSandboxer,
    service::Server,
    version,
};

#[tokio::main]
async fn main() {
    vmm_common::panic::set_panic_hook();
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    let config: Config<CloudHypervisorVMConfig> = Config::load_config(&args.config).await.unwrap();
    // Update args log level if it not presents args but in config.
    let log_level = args.log_level.unwrap_or(config.sandbox.log_level());
    let service_name = "kuasar-vmm-sandboxer-clh-service";
    trace::set_enabled(config.sandbox.enable_tracing);
    trace::setup_tracing(&log_level, service_name).unwrap();
    if (config.sandbox.snapshot.enable_environment_restore
        || config.sandbox.snapshot.enable_warmfork_restore)
        && config.template_pool.is_none()
    {
        log::error!(
            "Config error: snapshot restore requires a [template_pool] section in the config"
        );
        std::process::exit(1);
    }
    if config.sandbox.snapshot.enable_warmfork_restore
        && config.hypervisor.container_storage_backend != ContainerStorageBackend::VirtioBlk
    {
        log::error!("Config error: WarmFork restore requires container_storage_backend=virtio-blk");
        std::process::exit(1);
    }
    if config.sandbox.snapshot.enable_continuation_restore
        && config.hypervisor.container_storage_backend != ContainerStorageBackend::VirtioBlk
    {
        log::error!(
            "Config error: continuation snapshot restore requires container_storage_backend=virtio-blk"
        );
        std::process::exit(1);
    }
    vmm_sandboxer::utils::start_watchdog();
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        log::error!("failed to send ready notify: {}", e);
    }

    let snapshot_cfg = config.sandbox.snapshot.clone();
    let template_pool_cfg = config.template_pool.clone();

    // Capture the pool store_dir before template_pool_cfg is consumed, so that the continuation
    // store can share the same base when the pool is also enabled.
    let cont_store_base: std::path::PathBuf = if snapshot_cfg.enable_continuation_restore {
        match template_pool_cfg.as_ref() {
            Some(p) => std::path::PathBuf::from(&p.store_dir),
            None => {
                log::error!(
                    "Config error: enable_continuation_restore requires template_pool to be configured with a store_dir"
                );
                std::process::exit(1);
            }
        }
    } else {
        std::path::PathBuf::new()
    };

    let mut sandboxer: KuasarSandboxer<CloudHypervisorVMFactory, CloudHypervisorHooks> =
        KuasarSandboxer::new(
            config.sandbox,
            config.hypervisor,
            CloudHypervisorHooks::default(),
        );

    if let Some(pool_cfg) = template_pool_cfg.as_ref() {
        let gc_watermark = pool_cfg.gc_watermark.unwrap_or(50);
        let environment_max_per_key = pool_cfg
            .environment
            .as_ref()
            .and_then(|b| b.max_per_key)
            .unwrap_or(10);
        let warmfork_max_per_key = pool_cfg
            .warmfork
            .as_ref()
            .and_then(|c| c.max_per_key)
            .unwrap_or(10);
        let warmfork_lease_mode = pool_cfg
            .warmfork
            .as_ref()
            .map(|c| c.lease_mode.clone())
            .unwrap_or_default();

        // Fail-fast: validate oscillation constraints before starting.
        if let Some(min) = pool_cfg.environment.as_ref().and_then(|b| b.min_per_key) {
            if min > environment_max_per_key {
                log::error!(
                    "Config error: template_pool.environment.min_per_key ({}) > environment.max_per_key ({}). \
                     Pool will oscillate between refill and eviction. Aborting.",
                    min,
                    environment_max_per_key
                );
                std::process::exit(1);
            }
            if min > gc_watermark {
                log::error!(
                    "Config error: template_pool.environment.min_per_key ({}) > gc_watermark ({}). \
                     Pool will oscillate between refill and GC. Aborting.",
                    min,
                    gc_watermark
                );
                std::process::exit(1);
            }
        }

        if let Err(e) = sandboxer
            .init_template_pool(
                args.dir.clone(),
                pool_cfg.store_dir.clone().into(),
                environment_max_per_key,
                warmfork_max_per_key,
                gc_watermark,
                warmfork_lease_mode,
                snapshot_cfg.max_concurrent_restores,
            )
            .await
        {
            log::error!("Failed to initialize template pool: {}", e);
            std::process::exit(1);
        }
    }

    if snapshot_cfg.enable_continuation_restore {
        if let Err(e) = sandboxer.init_continuation_store(cont_store_base).await {
            log::error!("Failed to initialize continuation store: {}", e);
            std::process::exit(1);
        }
    }

    // Spawn the service API server.
    let admin_sock = args.admin_listen.clone();
    let service_h = sandboxer.service_handle(args.dir.clone());
    tokio::spawn(async move {
        Server::new(service_h, admin_sock).serve().await;
    });

    tokio::spawn(async move {
        signal::handle_signals(&log_level, service_name).await;
    });

    // Do recovery job
    if Path::new(&args.dir).exists() {
        sandboxer.recover(&args.dir).await;
    }

    if let Some(pool_cfg) = template_pool_cfg.as_ref() {
        if let Some(environment_cfg) = pool_cfg.environment.as_ref() {
            if let Some(depth) = environment_cfg.min_per_key {
                let interval = environment_cfg.maintenance_interval_secs.unwrap_or(60);
                sandboxer.start_maintenance_task(depth, interval);
            }
        }
    }

    if snapshot_cfg.enable_continuation_restore {
        let gc_interval = config
            .template_pool
            .as_ref()
            .and_then(|p| p.gc_interval_secs)
            .unwrap_or(60 * 60);
        sandboxer.start_continuation_gc_task(gc_interval);
    }

    // Run the sandboxer
    containerd_sandbox::run(
        "kuasar-vmm-sandboxer-clh",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();
}
