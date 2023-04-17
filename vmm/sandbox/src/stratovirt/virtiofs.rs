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

use std::process::Stdio;

use anyhow::{anyhow, Result};
use log::{debug, error};
use sandbox_derive::CmdLineParamSet;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::{process::Child, sync::watch::Sender, task::JoinHandle};

use crate::{
    param::ToCmdLineParams,
    utils::{read_std, write_file_atomic},
};

pub(crate) const DEFAULT_VHOST_USER_FS_BIN_PATH: &str = "/usr/bin/vhost_user_fs";

#[derive(CmdLineParamSet, Deserialize, Clone, Serialize, Debug)]
pub struct VirtiofsDaemon {
    #[param(ignore)]
    pub path: String,
    #[param(key = "D")]
    pub log_path: String,
    #[param(key = "socket-path")]
    pub socket_path: String,
    #[param(key = "source")]
    pub shared_dir: String,
    #[param(ignore)]
    pub pid: Option<u32>,
}

impl Default for VirtiofsDaemon {
    fn default() -> Self {
        Self {
            path: DEFAULT_VHOST_USER_FS_BIN_PATH.to_string(),
            log_path: "".to_string(),
            socket_path: "".to_string(),
            shared_dir: "".to_string(),
            pid: None,
        }
    }
}

impl VirtiofsDaemon {
    pub fn start(&mut self) -> Result<()> {
        let params = self.to_cmdline_params("-");
        let mut cmd = tokio::process::Command::new(&self.path);
        cmd.args(params.as_slice());
        debug!("start virtiofs daemon with cmdline: {:?}", cmd);
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn virtiofsd command: {}", e))?;
        self.pid = child.id();
        spawn_wait(child, "vhost_user_fs".to_string(), None, None);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        if let Some(pid) = self.pid {
            if pid > 1 {
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::SIGKILL,
                )?;
            } else {
                return Err(anyhow!("invalid virtiofs daemon process pid: {}", pid));
            }
        };

        Ok(())
    }
}

macro_rules! read_stdio {
    ($stdio:expr, $cmd_name:ident) => {
        if let Some(std) = $stdio {
            let cmd_name_clone = $cmd_name.clone();
            tokio::spawn(async move {
                read_std(std, &cmd_name_clone).await.unwrap_or_default();
            });
        }
    };
}

fn spawn_wait(
    child: Child,
    cmd_name: String,
    pid_file_path: Option<String>,
    exit_chan: Option<Sender<(u32, i128)>>,
) -> JoinHandle<()> {
    let mut child = child;
    tokio::spawn(async move {
        if let Some(pid_file) = pid_file_path {
            if let Some(pid) = child.id() {
                write_file_atomic(&pid_file, &pid.to_string())
                    .await
                    .unwrap_or_default();
            }
        }

        read_stdio!(child.stdout.take(), cmd_name);
        read_stdio!(child.stderr.take(), cmd_name);

        match child.wait().await {
            Ok(status) => {
                if !status.success() {
                    error!("{} exit {}", cmd_name, status);
                }
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((
                        status.code().unwrap_or_default() as u32,
                        now.unix_timestamp_nanos(),
                    ))
                    .unwrap_or_default();
                }
            }
            Err(e) => {
                error!("{} wait error {}", cmd_name, e);
                let now = OffsetDateTime::now_utc();
                if let Some(tx) = exit_chan {
                    tx.send((0, now.unix_timestamp_nanos())).unwrap_or_default();
                }
            }
        }
    })
}
