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

use anyhow::anyhow;
use containerd_sandbox::error;
use serde::de::DeserializeOwned;
use serde_derive::Deserialize;
use tokio::fs::read_to_string;

use crate::sandbox::SandboxConfig;

#[derive(Deserialize)]
pub struct Config<T> {
    pub sandbox: SandboxConfig,
    pub hypervisor: T,
}

impl<T: DeserializeOwned> Config<T> {
    pub async fn parse<P: AsRef<Path>>(path: P) -> error::Result<Self> {
        let toml_str = read_to_string(&path).await?;
        let conf: Self = toml::from_str(&toml_str)
            .map_err(|e| anyhow!("failed to parse kuasar sandboxer config {}", e))?;
        Ok(conf)
    }
}
