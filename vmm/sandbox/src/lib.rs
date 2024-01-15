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

use anyhow::Context;
use serde::de::DeserializeOwned;

use crate::{args::Args, config::Config, sandbox::KuasarSandbox};

#[macro_use]
mod device;

mod cgroup;
mod client;
mod container;
mod io;
mod network;
mod param;
mod storage;
mod vm;

pub mod args;
pub mod cloud_hypervisor;
pub mod config;
pub mod kata_config;
pub mod qemu;
pub mod sandbox;
pub mod stratovirt;
pub mod utils;
pub mod version;

async fn load_config<T: DeserializeOwned>(
    args: &Args,
    default_config_path: &str,
) -> anyhow::Result<(Config<T>, String)> {
    let mut config_path = default_config_path.to_string();
    let mut dir_path = String::new();
    if let Some(c) = &args.config {
        config_path = c.to_string();
    }
    if let Some(d) = &args.dir {
        dir_path = d.to_string();
        if !std::path::Path::new(&dir_path).exists() {
            tokio::fs::create_dir_all(&dir_path)
                .await
                .with_context(|| format!("Failed to mkdir for {}", dir_path))?;
        }
    }

    let path = std::path::Path::new(&config_path);
    let config: Config<T> = if path.exists() {
        Config::parse(path).await?
    } else {
        panic!("config file {} not exist", config_path);
    };
    Ok((config, dir_path))
}
