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

use std::{
    os::fd::{FromRawFd, IntoRawFd},
    process::Stdio,
    time::Duration,
};

use containerd_shim::{io_error, Error, Result};
use futures::StreamExt;
use log::{debug, error};
use nix::{
    pty::openpty,
    sys::{
        signal::{kill, Signal},
        termios::{tcgetattr, tcsetattr, LocalFlags, SetArg},
    },
    unistd::{dup, setsid, Pid},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{timeout, Instant},
};
use tokio_vsock::VsockStream;

use crate::{stream::RawStream, util::PidMonitorGuard, vsock::bind_vsock};

const EXEC_MODE_HEADER: &[u8] = b"KSR_MODE exec";
const MODE_HEADER_TIMEOUT: Duration = Duration::from_millis(100);
const MODE_HEADER_BUFFER_LIMIT: usize = 1024;

pub async fn listen_debug_console(addr: &str, debug_shell: &str) -> Result<()> {
    let l = bind_vsock(addr).await?;
    let shell = String::from(debug_shell);
    tokio::spawn(async move {
        let mut incoming = l.incoming();
        while let Some(Ok(s)) = incoming.next().await {
            debug!("get a debug console request");
            if let Err(e) = debug_console(s, &shell).await {
                error!("failed to open debug console {:?}", e);
            }
        }
    });

    Ok(())
}

pub async fn debug_console(mut stream: VsockStream, debug_shell: &str) -> Result<()> {
    let mut buf = Vec::new();
    let mut is_exec = false;
    let deadline = Instant::now() + MODE_HEADER_TIMEOUT;

    // Buffer the first line so exec-mode detection is stable even when the
    // protocol header arrives split across multiple reads.
    while buf.len() < MODE_HEADER_BUFFER_LIMIT {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        let mut chunk = [0u8; 128];
        match timeout(remaining, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(read_n)) => {
                buf.extend_from_slice(&chunk[..read_n]);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let header = &buf[..pos];
                    let header = header.strip_suffix(b"\r").unwrap_or(header);
                    if header == EXEC_MODE_HEADER {
                        is_exec = true;
                        buf.drain(..=pos);
                    }
                    break;
                }
            }
            Ok(Err(e)) => {
                return Err(Error::Other(format!(
                    "failed to read debug mode header: {e}"
                )))
            }
            Err(_) => break,
        }
    }

    let pty = openpty(None, None)?;
    let pty_master = pty.master;
    let pty_slave = pty.slave;

    if is_exec {
        // Disable echo so kuasar-ctl gets deterministic command output from the guest.
        let mut term = tcgetattr(&pty_slave).map_err(|e| Error::Other(e.to_string()))?;
        term.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&pty_slave, SetArg::TCSANOW, &term).map_err(|e| Error::Other(e.to_string()))?;
    }

    let mut cmd = std::process::Command::new(debug_shell);
    if is_exec {
        cmd.env("PS1", "");
    }
    let pty_fd = pty_slave.into_raw_fd();
    cmd.stdin(unsafe { Stdio::from_raw_fd(dup(pty_fd).map_err(|e| Error::Other(e.to_string()))?) });
    cmd.stdout(unsafe {
        Stdio::from_raw_fd(dup(pty_fd).map_err(|e| Error::Other(e.to_string()))?)
    });
    cmd.stderr(unsafe { Stdio::from_raw_fd(pty_fd) });
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
            Ok(())
        })
    };
    let mut monitor = PidMonitorGuard::subscribe().await?;
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            monitor.unsubscribe().await;
            return Err(Error::Other(format!("failed to spawn console: {}", e)));
        }
    };
    let (mut stream_reader, mut stream_writer) = stream.split();
    let pty_fd = RawStream::new(pty_master)
        .map_err(io_error!(e, "failed to create AsyncDirectFd from rawfd"))?;
    tokio::spawn(async move {
        let (mut pty_reader, mut pty_writer) = tokio::io::split(pty_fd);

        if !buf.is_empty() {
            if let Err(e) = pty_writer.write_all(&buf).await {
                error!("failed to replay data to pty: {:?}", e);
            }
        }

        tokio::select! {
            res = tokio::io::copy(&mut pty_reader, &mut stream_writer) => {
                debug!("pty closed: {:?}", res);
            }
            res = tokio::io::copy(&mut stream_reader, &mut pty_writer) => {
                debug!("stream closed: {:?}", res);
            }
        }
        let id = child.id();
        kill(Pid::from_raw(id as i32), Signal::SIGKILL).unwrap_or_default();
        let exit_status = monitor.wait(id as i32).await;
        debug!("debug console shell exit with {}", exit_status)
    });

    Ok(())
}
