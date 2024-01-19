/*
   Copyright The containerd Authors.

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
    io::{ErrorKind, IoSliceMut},
    ops::Deref,
    os::unix::prelude::{AsRawFd, FromRawFd, RawFd},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use containerd_shim::{
    asynchronous::{console::ConsoleSocket, processes::ProcessTemplate, util::asyncify},
    io::Stdio,
    io_error, other,
    util::IntoOption,
    Console, Error, ExitSignal, Result,
};
use log::{debug, error, warn};
use nix::{
    cmsg_space,
    sys::{
        socket::{recvmsg, ControlMessageOwned, MsgFlags, UnixAddr},
        termios::tcgetattr,
    },
};
use runc::io::{IOOption, Io, NullIo, PipedIo, FIFO};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
};
use tokio_vsock::{VsockListener, VsockStream};

use crate::{device::SYSTEM_DEV_PATH, vsock};

pub struct ProcessIO {
    pub uri: Option<String>,
    pub io: Option<Arc<dyn Io>>,
    pub copy: bool,
}

const VSOCK: &str = "vsock";

pub fn create_io(
    id: &str,
    io_uid: u32,
    io_gid: u32,
    stdio: &Stdio,
) -> containerd_shim::Result<ProcessIO> {
    if stdio.is_null() {
        let nio = NullIo::new().map_err(io_error!(e, "new Null Io"))?;
        let pio = ProcessIO {
            uri: None,
            io: Some(Arc::new(nio)),
            copy: false,
        };
        return Ok(pio);
    }
    let stdout = stdio.stdout.as_str();
    let scheme_path = stdout.trim().split("://").collect::<Vec<&str>>();
    let scheme: &str;
    let uri: String;
    if scheme_path.len() <= 1 {
        // no scheme specified
        // default schema to fifo
        uri = format!("fifo://{}", stdout);
        scheme = "fifo"
    } else {
        uri = stdout.to_string();
        scheme = scheme_path[0];
    }

    let mut pio = ProcessIO {
        uri: Some(uri),
        io: None,
        copy: false,
    };

    if scheme == "fifo" {
        debug!(
            "create named pipe io for container {}, stdin: {}, stdout: {}, stderr: {}",
            id,
            stdio.stdin.as_str(),
            stdio.stdout.as_str(),
            stdio.stderr.as_str()
        );
        let io = FIFO {
            stdin: stdio.stdin.to_string().none_if(|x| x.is_empty()),
            stdout: stdio.stdout.to_string().none_if(|x| x.is_empty()),
            stderr: stdio.stderr.to_string().none_if(|x| x.is_empty()),
        };
        pio.io = Some(Arc::new(io));
        pio.copy = false;
    } else if scheme.contains(VSOCK) {
        let opt = IOOption {
            open_stdin: !stdio.stdin.is_empty(),
            open_stdout: !stdio.stdout.is_empty(),
            open_stderr: !stdio.stderr.is_empty(),
        };
        let io = PipedIo::new(io_uid, io_gid, &opt).map_err(io_error!(e, "new PipedIo"))?;
        pio.io = Some(Arc::new(io));
        pio.copy = true;
    }
    Ok(pio)
}

pub(crate) async fn copy_io_or_console<P>(
    p: &mut ProcessTemplate<P>,
    socket: Option<ConsoleSocket>,
    pio: Option<ProcessIO>,
    exit_signal: Arc<ExitSignal>,
) -> Result<()> {
    if p.stdio.terminal {
        if let Some(console_socket) = socket {
            let console_result = copy_console(p, &console_socket, &p.stdio, exit_signal).await;
            console_socket.clean().await;
            match console_result {
                Ok(c) => {
                    p.console = Some(c);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    } else if let Some(pio) = pio {
        copy_io(&pio, &p.stdio, exit_signal).await?;
    }
    Ok(())
}

async fn copy_console<P>(
    p: &ProcessTemplate<P>,
    console_socket: &ConsoleSocket,
    stdio: &Stdio,
    exit_signal: Arc<ExitSignal>,
) -> Result<Console> {
    debug!("copy_console: waiting for runtime to send console fd");
    let stream = console_socket.accept().await?;
    let fd = asyncify(move || -> Result<RawFd> { receive_socket(stream.as_raw_fd()) }).await?;
    let f = unsafe { File::from_raw_fd(fd) };
    if !stdio.stdin.is_empty() {
        debug!("copy_console: pipe stdin to console");

        let stdin_clone = stdio.stdin.clone();
        let stdin_w = p.stdin.clone();
        // open the write side to make sure read side unblock, as open write side
        // will block too, open it in another thread
        tokio::spawn(async move {
            if let Ok(stdin_file) = OpenOptions::new().write(true).open(stdin_clone).await {
                let mut lock_guard = stdin_w.lock().await;
                *lock_guard = Some(stdin_file);
            }
        });

        let console_stdin = f
            .try_clone()
            .await
            .map_err(io_error!(e, "failed to clone console file"))?;
        spawn_copy_to(
            stdio.stdin.clone(),
            console_stdin,
            exit_signal.clone(),
            None::<fn()>,
        )
        .await?;
    }

    if !stdio.stdout.is_empty() {
        let console_stdout = f
            .try_clone()
            .await
            .map_err(io_error!(e, "failed to clone console file"))?;
        debug!("copy_console: pipe stdout from console");
        spawn_copy_from(
            console_stdout,
            stdio.stdout.clone(),
            exit_signal,
            None::<fn()>,
        )
        .await?;
    }
    let console = Console {
        file: f.into_std().await,
    };
    Ok(console)
}

pub async fn copy_io(pio: &ProcessIO, stdio: &Stdio, exit_signal: Arc<ExitSignal>) -> Result<()> {
    if !pio.copy {
        return Ok(());
    };
    if let Some(io) = &pio.io {
        if let Some(w) = io.stdin() {
            debug!("copy_io: pipe stdin from {}", stdio.stdin.as_str());
            if !stdio.stdin.is_empty() {
                spawn_copy_to(stdio.stdin.clone(), w, exit_signal.clone(), None::<fn()>).await?;
            }
        }

        if let Some(r) = io.stdout() {
            debug!("copy_io: pipe stdout from to {}", stdio.stdout.as_str());
            if !stdio.stdout.is_empty() {
                spawn_copy_from(r, stdio.stdout.clone(), exit_signal.clone(), None::<fn()>).await?;
            }
        }

        if let Some(r) = io.stderr() {
            if !stdio.stderr.is_empty() {
                spawn_copy_from(r, stdio.stderr.clone(), exit_signal.clone(), None::<fn()>).await?;
            }
        }
    }

    Ok(())
}

async fn spawn_copy_from<R, F>(
    from: R,
    to: String,
    exit_signal: Arc<ExitSignal>,
    on_close: Option<F>,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    let src = from;
    tokio::spawn(async move {
        let dst: Box<dyn AsyncWrite + Unpin + Send> = if to.contains(VSOCK) {
            tokio::select! {
                _ = exit_signal.wait() => {
                    debug!("container already exited, maybe nobody should connect vsock");
                    return;
                },
                res = VsockIo::new(&to, true) => {
                    match res {
                        Ok(v) => Box::new(v),
                        Err(e) => {
                            error!("failed to new vsock {}, {:?}", to, e);
                            return;
                        },
                    }
                }
            }
        } else {
            match OpenOptions::new().write(true).open(to.as_str()).await {
                Ok(f) => Box::new(f),
                Err(e) => {
                    error!("failed to get open file {}, {}", to, e);
                    return;
                }
            }
        };
        copy(src, dst, exit_signal, on_close).await;
        debug!("finished copy io from container to {}", to);
    });
    Ok(())
}

async fn spawn_copy_to<W, F>(
    from: String,
    to: W,
    exit_signal: Arc<ExitSignal>,
    on_close: Option<F>,
) -> Result<()>
where
    W: AsyncWrite + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    let dst = to;
    tokio::spawn(async move {
        let src: Box<dyn AsyncRead + Unpin + Send> = if from.contains(VSOCK) {
            tokio::select! {
                _ = exit_signal.wait() => {
                    debug!("container already exited, maybe nobody should connect");
                    return;
                },
                res = VsockIo::new(&from, true) => {
                    match res {
                        Ok(v) => Box::new(v),
                        Err(e) => {
                            error!("failed to new vsock {}, {:?}", from, e);
                            return;
                        },
                    }
                }
            }
        } else {
            match OpenOptions::new().read(true).open(from.as_str()).await {
                Ok(f) => Box::new(f),
                Err(e) => {
                    error!("failed to get open file {}, {}", from, e);
                    return;
                }
            }
        };
        copy(src, dst, exit_signal, on_close).await;
        debug!("finished copy io from {} to container", from);
    });
    Ok(())
}

async fn copy<R, W, F>(mut src: R, mut dst: W, exit_signal: Arc<ExitSignal>, on_close: Option<F>)
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    tokio::select! {
        _ = exit_signal.wait() => {
            debug!("container exit, copy task should exit too");
        },
        res = io_copy(&mut src, &mut dst) => {
           if let Err(e) = res {
                error!("copy io failed {}", e);
            }
        }
    }
    if let Some(f) = on_close {
        f();
    }
}

async fn io_copy<'a, R, W>(src: &'a mut R, dst: &'a mut W) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = [0u8; 4096];
    let mut written = 0;

    loop {
        let len = match src.read(&mut buf).await {
            Ok(0) => return Ok(written),
            Ok(len) => len,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        written += len as u64;
        match dst.write_all(&buf[..len]).await {
            Ok(_) => {
                dst.flush().await.unwrap_or_default();
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn receive_socket(stream_fd: RawFd) -> containerd_shim::Result<RawFd> {
    let mut buf = [0u8; 4096];
    let mut iovec = [IoSliceMut::new(&mut buf)];
    let mut space = cmsg_space!([RawFd; 2]);
    let (path, fds) =
        match recvmsg::<UnixAddr>(stream_fd, &mut iovec, Some(&mut space), MsgFlags::empty()) {
            Ok(msg) => {
                let mut iter = msg.cmsgs();
                if let Some(ControlMessageOwned::ScmRights(fds)) = iter.next() {
                    (iovec[0].deref(), fds)
                } else {
                    return Err(other!("received message is empty"));
                }
            }
            Err(e) => {
                return Err(other!("failed to receive message: {}", e));
            }
        };
    if fds.is_empty() {
        return Err(other!("received message is empty"));
    }
    let path = String::from_utf8(Vec::from(path)).unwrap_or_else(|e| {
        warn!("failed to get path from array {}", e);
        "".to_string()
    });
    let path = path.trim_matches(char::from(0));
    debug!(
        "copy_console: console socket get path: {}, fd: {}",
        path, &fds[0]
    );
    tcgetattr(fds[0])?;
    Ok(fds[0])
}

// TODO we still have to create pipes, otherwise the device maybe opened multiple times in container,
// 1. the processes in container reopen the stdio files by /proc/self/fd/0, /proc/self/fd/1 or
//    /proc/self/fd/2, it may has no permission to open it because of the devices cgroup constraint
// 2. even we give container the permission to open this file, the serial device can not
//    be opened multiple times, it will return the error of "Device or resource busy".
pub async fn convert_stdio(stdio: &Stdio) -> containerd_shim::Result<Stdio> {
    Ok(Stdio {
        stdin: get_io_file_name(&stdio.stdin).await?,
        stdout: get_io_file_name(&stdio.stdout).await?,
        stderr: get_io_file_name(&stdio.stderr).await?,
        terminal: stdio.terminal,
    })
}

async fn get_io_file_name(name: &str) -> containerd_shim::Result<String> {
    if !name.is_empty() && !name.contains(VSOCK) {
        find_serial_dev(name).await
    } else {
        Ok(name.to_string())
    }
}

async fn find_serial_dev(serial_name: &str) -> Result<String> {
    let mut virtio_ports = tokio::fs::read_dir("/sys/class/virtio-ports")
        .await
        .unwrap();
    while let Some(virtio_port) = virtio_ports.next_entry().await.unwrap() {
        let path = virtio_port.path();
        debug!("get path: {}", path.to_string_lossy());
        let abs = PathBuf::new().join(path.as_path()).join("name");
        if abs.exists() {
            let name = tokio::fs::read_to_string(abs.as_path()).await.unwrap();
            debug!("read name:{} from abspath {}", name, abs.to_string_lossy());
            if name.trim() == serial_name {
                let dev_path = format!(
                    "{}/{}",
                    SYSTEM_DEV_PATH,
                    path.file_name().unwrap().to_string_lossy()
                );
                return Ok(dev_path);
            }
        }
    }
    Err(other!("failed to get serial dev of name {}", serial_name))
}

pin_project_lite::pin_project! {
    pub struct VsockIo {
        #[pin]
        l: VsockListener,
        stream: Option<VsockStream>,
    }
}

macro_rules! _accept_stream {
    ($this:ident, $cx:ident) => {
        *$this.stream = match $this.l.poll_accept($cx) {
            Poll::Ready(s) => match s {
                Ok(s) => Some(s.0),
                Err(e) => return Poll::Ready(Err(e)),
            },
            Poll::Pending => {
                return Poll::Pending;
            }
        }
    };
}

macro_rules! _do_poll_on_stream {
    ($this:ident, $do_poll:expr) => {
        match $do_poll {
            Poll::Ready(res) => match res {
                Ok(t) => {
                    return Poll::Ready(Ok(t));
                }
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        *$this.stream = None;
                    } else {
                        return Poll::Ready(Err(e));
                    }
                }
            },
            Poll::Pending => {
                return Poll::Pending;
            }
        }
    };
}

macro_rules! poll_loop {
    ($this:ident, $cx:ident, $buf:ident, $func:ident) => {
        loop {
            match $this.stream {
                None => {
                    _accept_stream!($this, $cx);
                }
                Some(s) => {
                    _do_poll_on_stream!($this, Pin::new(s).$func($cx, $buf));
                }
            }
        }
    };

    ($this:ident, $cx:ident, $func:ident) => {
        loop {
            match $this.stream {
                None => {
                    _accept_stream!($this, $cx);
                }
                Some(s) => {
                    _do_poll_on_stream!($this, Pin::new(s).$func($cx));
                }
            }
        }
    };
}

impl AsyncRead for VsockIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        poll_loop!(this, cx, buf, poll_read)
    }
}

impl AsyncWrite for VsockIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut this = self.project();
        poll_loop!(this, cx, buf, poll_write)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        poll_loop!(this, cx, poll_flush)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut this = self.project();
        poll_loop!(this, cx, poll_shutdown)
    }
}

impl VsockIo {
    pub async fn new(addr: &str, bind: bool) -> Result<Self> {
        let l = vsock::bind_vsock(addr).await?;
        let mut vio = VsockIo { l, stream: None };
        if bind {
            let (stream, _) = vio
                .l
                .accept()
                .await
                .map_err(io_error!(e, "failed to accept"))?;
            vio.stream = Some(stream);
        }
        Ok(vio)
    }
}
