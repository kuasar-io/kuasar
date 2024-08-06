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

use anyhow::anyhow;
use containerd_sandbox::error::Result;
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
    pub fn new(sandbox: SandboxConfig, hypervisor: T) -> Self {
        Self {
            sandbox,
            hypervisor,
        }
    }

    // Load config from configuration file, args in command line will override args in file
    pub async fn load_config(config_path: &str) -> Result<Self> {
        if config_path.is_empty() {
            return Err(anyhow!("config path is empty").into());
        }
        // Parse config from toml file
        let toml_str = read_to_string(config_path).await?;

        let config = toml::from_str(&toml_str)
            .map_err(|e| anyhow!("failed to parse kuasar sandboxer config: {}", e))?;
        Ok(config)
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::Path;

    use containerd_sandbox::error::Result;
    use containerd_shim::util::write_str_to_file;
    use serde_derive::Deserialize;
    use temp_dir::TempDir;

    use crate::{config::Config, sandbox::SandboxConfig};

    #[derive(Deserialize)]
    struct MockHypervisor {
        path: String,
    }

    #[test]
    fn test_config_new() {
        let mut sandbox_config = SandboxConfig::default();
        sandbox_config.log_level = "debug".to_string();
        sandbox_config.enable_tracing = false;
        let mock_path = "/usr/local/bin/mock-hypervisor";
        let mock_config = MockHypervisor {
            path: mock_path.to_string(),
        };
        let config = Config::new(sandbox_config, mock_config);

        assert_eq!(config.sandbox.log_level, "debug");
        assert_eq!(config.sandbox.enable_tracing, false);
        assert_eq!(config.hypervisor.path, mock_path);
    }

    #[tokio::test]
    async fn test_config_load_empty_dir() {
        let res: Result<Config<MockHypervisor>> = Config::load_config("").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_config_load() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = Path::join(tmp_dir.path(), "config_clh.toml");

        let toml_str = "
[sandbox]
log_level = \"debug\"
enable_tracing = false
[hypervisor]
path = \"/usr/local/bin/mock-hypervisor\"
";
        write_str_to_file(tmp_path.as_path(), toml_str)
            .await
            .unwrap();

        let config: Config<MockHypervisor> = Config::load_config(tmp_path.to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(config.sandbox.log_level, "debug");
        assert_eq!(config.sandbox.enable_tracing, false);
        assert_eq!(config.hypervisor.path, "/usr/local/bin/mock-hypervisor");
    }

    #[tokio::test]
    async fn test_config_load_with_wrong_config() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = Path::join(tmp_dir.path(), "config_clh.toml");

        let toml_str = "
[sandbox]
log_level = \"\"
[hypervisor]
no_key = 0
";
        write_str_to_file(tmp_path.as_path(), toml_str)
            .await
            .unwrap();

        let res: Result<Config<MockHypervisor>> =
            Config::load_config(tmp_path.to_str().unwrap()).await;

        assert!(res.is_err())
    }
}
