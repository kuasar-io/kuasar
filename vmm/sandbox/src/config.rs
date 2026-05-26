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

use crate::sandbox::{SandboxConfig, TemplateLeaseMode};

#[derive(Deserialize)]
pub struct Config<T> {
    pub sandbox: SandboxConfig,
    pub hypervisor: T,
    #[serde(default)]
    pub template_pool: Option<TemplatePoolConfig>,
}

/// Top-level template pool configuration.
///
/// Shared across all template kinds. Kind-specific tuning lives in the
/// `[template_pool.environment]` and `[template_pool.warmfork]` sub-sections.
#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TemplatePoolConfig {
    /// Directory where template snapshots are persisted across restarts.
    pub store_dir: String,
    /// Background GC interval in seconds for pool-level GC and continuation GC.
    #[serde(default)]
    pub gc_interval_secs: Option<u64>,
    /// GC watermark: when the total number of pool-resident templates reaches
    /// this value, the oldest idle templates are evicted to make room.
    /// Applies to Environment templates (always evictable) and WarmFork Shared templates
    /// (evicted only when ref_count == 0).
    #[serde(default)]
    pub gc_watermark: Option<usize>,
    /// Environment-specific pool settings.
    #[serde(default)]
    pub environment: Option<EnvironmentPoolConfig>,
    /// WarmFork-specific pool settings.
    #[serde(default)]
    pub warmfork: Option<WarmForkPoolConfig>,
}

/// Environment pool settings (bare VM snapshot pool).
///
/// Each template is a snapshot of a booted VM with no containers — the base layer
/// on which WarmFork templates are built.  Templates are automatically refilled by
/// the background maintenance task and always use symlink on restore.
#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPoolConfig {
    /// Minimum Environment templates to keep per TemplateKey.
    /// When set, the background maintenance task is automatically enabled and
    /// refills the pool whenever the count falls below this value.
    #[serde(default)]
    pub min_per_key: Option<usize>,
    /// Maximum Environment templates retained per TemplateKey; oldest evicted when exceeded.
    /// Must satisfy: min_per_key ≤ max_per_key ≤ gc_watermark.
    #[serde(default)]
    pub max_per_key: Option<usize>,
    /// Background maintenance check interval in seconds. Default: 60.
    #[serde(default)]
    pub maintenance_interval_secs: Option<u64>,
}

/// WarmFork pool settings.
///
/// WarmFork templates are manually created via the admin API.
#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WarmForkPoolConfig {
    /// How Container template disk images are duplicated on restore.
    /// - `shared`: reflink COW copy (requires XFS/btrfs with reflink support).
    /// - `exclusive`: symlink (single sandbox ownership; template removed after use).
    ///   Environment templates are unaffected by this setting (always use symlink).
    #[serde(default)]
    pub lease_mode: TemplateLeaseMode,
    /// Maximum Container templates retained per TemplateKey; oldest evicted when exceeded.
    #[serde(default)]
    pub max_per_key: Option<usize>,
}

impl<T: DeserializeOwned> Config<T> {
    pub fn new(sandbox: SandboxConfig, hypervisor: T) -> Self {
        Self {
            sandbox,
            hypervisor,
            template_pool: None,
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

    use crate::{
        config::Config,
        sandbox::{SandboxConfig, TemplateLeaseMode},
    };

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
    async fn test_config_load_with_template_pool() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = Path::join(tmp_dir.path(), "config_pool.toml");

        let toml_str = r#"
[sandbox]
log_level = "info"
enable_tracing = false

[sandbox.snapshot]
enable_continuation_restore = true
max_concurrent_restores = 8

[hypervisor]
path = "/usr/local/bin/mock-hypervisor"

[template_pool]
store_dir = "/var/lib/kuasar/pool"
gc_interval_secs = 45
gc_watermark = 20

[template_pool.environment]
min_per_key = 2
max_per_key = 5
maintenance_interval_secs = 30

[template_pool.warmfork]
lease_mode = "exclusive"
max_per_key = 3
"#;
        write_str_to_file(tmp_path.as_path(), toml_str)
            .await
            .unwrap();

        let config: Config<MockHypervisor> = Config::load_config(tmp_path.to_str().unwrap())
            .await
            .unwrap();

        assert!(config.sandbox.snapshot.enable_continuation_restore);
        assert_eq!(config.sandbox.snapshot.max_concurrent_restores, 8);

        let pool = config
            .template_pool
            .expect("template_pool should be present");
        assert_eq!(pool.store_dir, "/var/lib/kuasar/pool");
        assert_eq!(pool.gc_interval_secs, Some(45));
        assert_eq!(pool.gc_watermark, Some(20));

        let bv = pool
            .environment
            .expect("environment section should be present");
        assert_eq!(bv.min_per_key, Some(2));
        assert_eq!(bv.max_per_key, Some(5));
        assert_eq!(bv.maintenance_interval_secs, Some(30));

        let ct = pool.warmfork.expect("warmfork section should be present");
        assert!(matches!(ct.lease_mode, TemplateLeaseMode::Exclusive));
        assert_eq!(ct.max_per_key, Some(3));
    }

    #[tokio::test]
    async fn test_config_load_template_pool_defaults() {
        let tmp_dir = TempDir::new().unwrap();
        let tmp_path = Path::join(tmp_dir.path(), "config_pool_defaults.toml");

        // Minimal template_pool — all optional fields absent
        let toml_str = r#"
[sandbox]
log_level = "info"
enable_tracing = false

[hypervisor]
path = "/usr/local/bin/mock-hypervisor"

[template_pool]
store_dir = "/var/lib/kuasar/pool"
"#;
        write_str_to_file(tmp_path.as_path(), toml_str)
            .await
            .unwrap();

        let config: Config<MockHypervisor> = Config::load_config(tmp_path.to_str().unwrap())
            .await
            .unwrap();

        let pool = config
            .template_pool
            .expect("template_pool should be present");
        assert_eq!(pool.gc_watermark, None);
        assert!(pool.environment.is_none());
        assert!(pool.warmfork.is_none());
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
