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

use std::{fs::FileType, os::unix::fs::FileTypeExt, path::Path, process::Stdio};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use tokio::process::Command;

pub async fn is_block_device<P: AsRef<Path>>(path: P) -> Result<bool> {
    let file_type = match get_file_type(path).await {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    Ok(file_type.is_block_device())
}

pub async fn is_regular_file<P: AsRef<Path>>(path: P) -> Result<bool> {
    let file_type = get_file_type(path).await?;
    Ok(file_type.is_file()
        && !file_type.is_block_device()
        && !file_type.is_char_device()
        && !file_type.is_fifo()
        && !file_type.is_socket())
}

pub async fn get_file_type<P: AsRef<Path>>(path: P) -> Result<FileType> {
    let real_path = match tokio::fs::canonicalize(path).await {
        Ok(rp) => rp,
        Err(e) => {
            return Err(e.into());
        }
    };

    let md = tokio::fs::metadata(&real_path)
        .await
        .map_err(|e| anyhow!("can not get metadata of {}, {}", real_path.display(), e))?;
    Ok(md.file_type())
}

pub async fn get_fstype(path: &str) -> Result<String> {
    let output = Command::new("blkid")
        .args(["-p", "-s", "TYPE", "-s", "PTTYPE", "-o", "export", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("failed to execute command {}", e))?;
    if !output.status.success() {
        let error_msg = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "failed to execute command blkid, exit code: {}, error message: {}",
            output.status,
            error_msg
        )
        .into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for l in stdout.lines() {
        let kv: Vec<&str> = l.split('=').collect();
        if kv.len() != 2 {
            continue;
        }
        if kv[0] == "TYPE" {
            return Ok(kv[1].to_string());
        }
    }
    Err(anyhow!("failed to get fstype of {}", path).into())
}
