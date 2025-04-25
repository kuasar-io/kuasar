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

use containerd_shim::{io_error, Error, Result};
use tokio::fs::read_to_string;

const SHAREFS_TYPE: &str = "task.sharefs_type";
const LOG_LEVEL: &str = "task.log_level";
const TASK_DEBUG: &str = "task.debug";
const ENABLE_TRACING: &str = "task.enable_tracing";
const DEBUG_SHELL: &str = "task.debug_shell";

#[cfg(feature = "image-service")]
const AA_KBC_PARAMS: &str = "task.aa_kbc_params";
#[cfg(feature = "image-service")]
const HTTPS_PROXY: &str = "task.https_proxy";
#[cfg(feature = "image-service")]
const NO_PROXY: &str = "task.no_proxy";
#[cfg(feature = "image-service")]
const ENABLE_SIGNATURE_VERIFICATION: &str = "task.enable_signature_verification";
#[cfg(feature = "image-service")]
const IMAGE_POLICY_FILE: &str = "task.image_policy";
#[cfg(feature = "image-service")]
const IMAGE_REGISTRY_AUTH_FILE: &str = "task.image_registry_auth";
#[cfg(feature = "image-service")]
const SIMPLE_SIGNING_SIGSTORE_CONFIG: &str = "task.simple_signing_sigstore_config";

macro_rules! parse_cmdline {
    ($param:ident, $key:ident, $field:expr) => {
        if $param.len() == 1 && $param[0] == $key {
            $field = true;
            continue;
        }
    };
    ($param:ident, $key:ident, $field:expr, $func:path) => {
        if $param.len() == 2 && $param[0] == $key {
            let val = $func($param[1]);
            $field = val;
            continue;
        }
    };
}

#[derive(Debug)]
pub struct ImageConfig {
    pub(crate) aa_kbc_params: String,
    pub(crate) https_proxy: String,
    pub(crate) no_proxy: String,
    pub(crate) enable_signature_verification: bool,
    pub(crate) image_policy_file: String,
    pub(crate) image_registry_auth_file: String,
    pub(crate) simple_signing_sigstore_config: String,
}

#[derive(Debug)]
pub struct TaskConfig {
    pub(crate) sharefs_type: String,
    pub(crate) log_level: String,
    pub(crate) debug: bool,
    pub(crate) enable_tracing: bool,
    pub(crate) debug_shell: String,
    #[cfg(feature = "image-service")]
    pub(crate) image_config: ImageConfig,
}

impl Default for TaskConfig {
    fn default() -> Self {
        TaskConfig {
            sharefs_type: "9p".to_string(),
            log_level: "info".to_string(),
            debug: false,
            enable_tracing: false,
            debug_shell: "/bin/bash".to_string(),
            #[cfg(feature = "image-service")]
            image_config: ImageConfig {
                aa_kbc_params: "".to_string(),
                https_proxy: "".to_string(),
                no_proxy: "".to_string(),
                enable_signature_verification: false,
                image_policy_file: "".to_string(),
                image_registry_auth_file: "".to_string(),
                simple_signing_sigstore_config: "".to_string(),
            }
        }
    }
}

impl TaskConfig {
    pub async fn new() -> Result<Self> {
        let mut config = TaskConfig::default();
        let cmdline = read_to_string("/proc/cmdline")
            .await
            .map_err(io_error!(e, "failed to open /proc/cmdline"))?;
        let params: Vec<&str> = cmdline.split_ascii_whitespace().collect();
        for p in params {
            let param: Vec<&str> = p.split('=').collect();
            parse_cmdline!(param, SHAREFS_TYPE, config.sharefs_type, String::from);
            parse_cmdline!(param, LOG_LEVEL, config.log_level, String::from);
            parse_cmdline!(param, TASK_DEBUG, config.debug);
            parse_cmdline!(param, ENABLE_TRACING, config.enable_tracing);
            parse_cmdline!(param, DEBUG_SHELL, config.debug_shell, String::from);

            #[cfg(feature = "image-service")]
            {
                parse_cmdline!(param, AA_KBC_PARAMS, config.image_config.aa_kbc_params, String::from);
                parse_cmdline!(param, HTTPS_PROXY, config.image_config.https_proxy, String::from);
                parse_cmdline!(param, NO_PROXY, config.image_config.no_proxy, String::from);
                parse_cmdline!(param, ENABLE_SIGNATURE_VERIFICATION, config.image_config.enable_signature_verification);
                // URI of the image security file
                parse_cmdline!(param, IMAGE_POLICY_FILE, config.image_config.image_policy_file, String::from);
                // URI of the registry auth file
                parse_cmdline!(param, IMAGE_REGISTRY_AUTH_FILE, config.image_config.image_registry_auth_file, String::from);
                // URI of the simple signing sigstore file
                // used when simple signing verification is used
                parse_cmdline!(param, SIMPLE_SIGNING_SIGSTORE_CONFIG, config.image_config.simple_signing_sigstore_config, String::from);
            }
        }
        Ok(config)
    }
}
