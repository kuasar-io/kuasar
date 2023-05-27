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

use std::{os::unix::prelude::RawFd, path::Path};

use anyhow::anyhow;
use containerd_sandbox::{
    error::{Error, Result},
    spec::Mount,
};
use containerd_shim::util::IntoOption;
use log::{debug, error};
use nix::{
    errno::Errno,
    libc,
    sys::socket::{bind, socket, AddressFamily, SockAddr, SockFlag, SockType, UnixAddr},
    unistd::close,
    NixPath,
};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

pub const MNT_NOFOLLOW: i32 = 0x8;

pub async fn write_file_atomic<P: AsRef<Path>>(path: P, s: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let file = path
        .file_name()
        .ok_or(Error::InvalidArgument(String::from("pid path illegal")))?;
    let tmp_path = path
        .parent()
        .map(|x| x.join(format!(".{}", file.to_str().unwrap_or(""))))
        .ok_or(Error::InvalidArgument(String::from(
            "failed to create tmp path",
        )))?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .await
        .map_err(|e| anyhow!("failed to open path {}, {}", tmp_path.display(), e))?;
    f.write_all(s).await.map_err(|e| {
        anyhow!(
            "failed to write string to path {}, {}",
            tmp_path.display(),
            e
        )
    })?;
    f.sync_data()
        .await
        .map_err(|e| anyhow!("failed to sync data to path {}, {}", tmp_path.display(), e))?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| anyhow!("failed to rename file:{}, {}", tmp_path.display(), e).into())
}

pub fn _bind_socket(socket_path: &str) -> Result<RawFd> {
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(|e| anyhow!("failed to create raw socket: {}", e))?;
    let unix_addr = UnixAddr::new(socket_path).map_err(|e| {
        close(fd).unwrap_or_default();
        anyhow!("failed to new socket: {}", e)
    })?;
    let sockaddr = SockAddr::Unix(unix_addr);
    bind(fd, &sockaddr).map_err(|e| {
        close(fd).unwrap_or_default();
        anyhow!("failed to bind unix socket: {}", e)
    })?;
    Ok(fd)
}

pub async fn mount_rootfs(m: &Mount, target: impl AsRef<Path>) -> Result<()> {
    let fs_type = m.r#type.as_str().none_if(|x| x.is_empty());
    let source = m.source.as_str().none_if(|x| x.is_empty());
    let options = m.options.as_slice();
    let target = target.as_ref();
    containerd_shim::mount::mount_rootfs(fs_type, source, options, target)
        .map_err(|_e| anyhow!("failed to mount {:?} to {}", m, target.display()))?;
    Ok(())
}

pub async fn cleanup_mounts(base_dir: &str) -> Result<()> {
    let mounts = tokio::fs::read_to_string("/proc/mounts")
        .await
        .map_err(|e| anyhow!("failed to read /proc/mounts,{}", e))?;
    for line in mounts.lines() {
        let fields = line.split_whitespace().collect::<Vec<&str>>();
        let path = fields[1];
        if path.starts_with(base_dir) {
            unmount(path, libc::MNT_DETACH | MNT_NOFOLLOW).unwrap_or_else(|e| {
                error!("failed to remove {}, err: {}", path, e);
            });
        }
    }
    Ok(())
}

pub fn unmount(target: &str, flags: i32) -> Result<()> {
    let res = target
        .with_nix_path(|cstr| unsafe { libc::umount2(cstr.as_ptr(), flags) })
        .map_err(|e| anyhow!("failed to umount {}, {}", target, e))?;
    let err = Errno::result(res).map(drop);
    match err {
        Ok(_) => Ok(()),
        Err(e) => {
            if e == Errno::ENOENT {
                debug!("the umount path {} not exist", target);
                return Ok(());
            }
            Err(anyhow!("failed to umount {}, {}", target, e).into())
        }
    }
}
