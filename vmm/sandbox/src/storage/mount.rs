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
use containerd_sandbox::{error::Result, spec::Mount};
use vmm_common::DEV_SHM;

use crate::{storage::MountInfo, utils::read_file};

pub fn is_bind_shm(m: &Mount) -> bool {
    is_bind(m) && m.destination == DEV_SHM
}

pub fn is_bind(m: &Mount) -> bool {
    m.r#type == "bind" && !m.source.is_empty()
}

pub fn is_overlay(m: &Mount) -> bool {
    m.r#type == "overlay"
}

pub async fn get_mount_info(mount_point: &str) -> Result<Option<MountInfo>> {
    if mount_point.is_empty() {
        return Ok(None);
    }
    let mounts = read_file("/proc/mounts").await?;
    for line in mounts.lines() {
        let fields = line.split_whitespace().collect::<Vec<&str>>();
        if fields.len() < 4 {
            return Err(anyhow!("the line '{}' in /proc/mounts has format error", line).into());
        }
        // format: "/dev/sdc /mnt ext4 rw,relatime,stripe=64 0 0"
        let mp = fields[1].to_string();
        if mp == mount_point {
            let device = fields[0].to_string();
            let fs_type = fields[2].to_string();
            let options = fields[3].split(',').map(|x| x.to_string()).collect();
            return Ok(Some(MountInfo {
                device,
                mount_point: mp,
                fs_type,
                options,
            }));
        }
    }
    Ok(None)
}
