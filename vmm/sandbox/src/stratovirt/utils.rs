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

use std::time::Duration;

use anyhow::anyhow;
use containerd_sandbox::error::{Error, Result};

use crate::utils::read_file;

pub(crate) async fn detect_pid(path: &str, bin_path: &str) -> Result<u32> {
    let mut err = None;
    for _i in 0..1000 {
        let pid_res = read_file(path).await.and_then(|x| {
            x.trim()
                .parse::<u32>()
                .map_err(|e| anyhow!("failed to parse stratovirt.pid {}", e).into())
        });

        match pid_res {
            Ok(pid) => {
                let p = procfs::process::Process::new(pid as i32)
                    .map_err(|e| anyhow!("failed to get stratovirt process, {}", e))?;
                // make sure the command is bin_path,
                // because sometimes os may reuse the pid to launch other processes
                let cmd = p
                    .cmdline()
                    .map_err(|e| anyhow!("failed to get command from stratovirt, {}", e))?;
                let mut is_bin = false;
                for s in cmd {
                    if s.contains(bin_path) {
                        is_bin = true
                    }
                }
                if is_bin {
                    return Ok(pid);
                } else {
                    return Err(Error::NotFound(format!("stratovirt process {}", pid)));
                }
            }
            Err(e) => {
                err = Some(e);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    Err(anyhow!("timeout waiting for the pid file, err: {:?}", err).into())
}
