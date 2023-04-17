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

use std::{os::unix::prelude::FromRawFd, process::Stdio};

use containerd_shim::{
    io_error,
    monitor::{monitor_subscribe, Topic},
    other_error, Error, Result,
};
use futures::StreamExt;
use log::{debug, error};
use nix::{pty::openpty, unistd::setsid};
use tokio::process::Command;
use tokio_vsock::VsockStream;

use crate::{stream::RawStream, util::wait_pid, vsock::bind_vsock};

pub async fn listen_debug_console(addr: &str) -> Result<()> {
    let l = bind_vsock(addr).await?;
    tokio::spawn(async move {
        let mut incoming = l.incoming();
        while let Some(Ok(s)) = incoming.next().await {
            if let Err(e) = debug_console(s).await {
                error!("failed to open debug console {:?}", e);
            }
        }
    });

    Ok(())
}

pub async fn debug_console(stream: VsockStream) -> Result<()> {
    let pty = openpty(None, None)?;
    let pty_master = pty.master;
    let mut cmd = Command::new("/bin/bash");
    cmd.stdin(unsafe { Stdio::from_raw_fd(pty.slave) });
    cmd.stdout(unsafe { Stdio::from_raw_fd(pty.slave) });
    cmd.stderr(unsafe { Stdio::from_raw_fd(pty.slave) });
    unsafe {
        cmd.pre_exec(move || {
            setsid()?;
            Ok(())
        })
    };
    let s = monitor_subscribe(Topic::Pid).await?;
    let child = cmd
        .spawn()
        .map_err(other_error!(e, "failed to spawn console"))?;
    let (mut stream_reader, mut stream_writer) = stream.split();
    let pty_fd = RawStream::new(pty_master)
        .map_err(io_error!(e, "failed to create AsyncDirectFd from rawfd"))?;
    tokio::spawn(async move {
        let (mut pty_reader, mut pty_writer) = tokio::io::split(pty_fd);
        tokio::select! {
            res = tokio::io::copy(&mut pty_reader, &mut stream_writer) => {
                debug!("pty closed: {:?}", res);
            }
            res = tokio::io::copy(&mut stream_reader, &mut pty_writer) => {
                debug!("stream closed: {:?}", res);
            }
        }
        if let Some(id) = child.id() {
            let exit_status = wait_pid(id as i32, s).await;
            debug!("debug console shell exit with {}", exit_status)
        }
    });

    Ok(())
}
