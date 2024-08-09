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
pub struct TaskConfig {
    pub(crate) sharefs_type: String,
    pub(crate) log_level: String,
    pub(crate) debug: bool,
}

impl Default for TaskConfig {
    fn default() -> Self {
        TaskConfig {
            sharefs_type: "9p".to_string(),
            log_level: "info".to_string(),
            debug: false,
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
        }
        Ok(config)
    }
}
