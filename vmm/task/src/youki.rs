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
    convert::TryFrom,
    os::{
        fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::fs::fchown,
    },
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest, Options, Status},
    asynchronous::{
        console::ConsoleSocket,
        container::{ContainerFactory, ContainerTemplate, ProcessFactory},
        processes::{ProcessLifecycle, ProcessTemplate},
    },
    io::Stdio,
    io_error, other, other_error,
    protos::{
        api::ProcessInfo,
        cgroups::metrics::Metrics,
        protobuf::{CodedInputStream, Message},
    },
    util::read_spec,
    Error, ExitSignal, Result,
};
use libcontainer::{
    container::{builder::ContainerBuilder, Container},
    error::LibcontainerError,
    signal::Signal,
    syscall::syscall::SyscallType,
};
use log::{debug, warn};
use nix::{sys::signal::kill, unistd::Pid};
use oci_spec::runtime::{LinuxResources, Process, Spec};
use runc::io::{IOOption, Io, NullIo};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Command,
    sync::Mutex,
    task::spawn_blocking,
};
use vmm_common::{
    storage::{Storage, ANNOTATION_KEY_STORAGE},
    KUASAR_STATE_DIR,
};

use crate::{
    device::rescan_pci_bus,
    io::{convert_stdio, copy_io_or_console, ProcessIO},
    sandbox::SandboxResources,
    util::{read_io, read_storages},
};

pub type ExecProcess = ProcessTemplate<YoukiExecLifecycle>;
pub type InitProcess = ProcessTemplate<YoukiInitLifecycle>;

pub type YoukiContainer = ContainerTemplate<InitProcess, ExecProcess, YoukiExecFactory>;

pub const YOUKI_DIR: &str = "/run/kuasar/youki";

#[derive(Clone)]
pub(crate) struct YoukiFactory {
    sandbox: Arc<Mutex<SandboxResources>>,
}

#[async_trait]
impl ContainerFactory<YoukiContainer> for YoukiFactory {
    async fn create(
        &self,
        _ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<YoukiContainer> {
        rescan_pci_bus().await?;
        let bundle = format!("{}/{}", KUASAR_STATE_DIR, req.id);
        let spec: Spec = read_spec(&bundle).await?;
        let annotations = spec.annotations().clone().unwrap_or_default();
        let storages = if let Some(storage_str) = annotations.get(ANNOTATION_KEY_STORAGE) {
            serde_json::from_str::<Vec<Storage>>(storage_str)?
        } else {
            read_storages(&bundle, req.id()).await?
        };
        self.sandbox
            .lock()
            .await
            .add_storages(req.id(), storages)
            .await?;
        let mut opts = Options::new();
        if let Some(any) = req.options.as_ref() {
            let mut input = CodedInputStream::from_bytes(any.value.as_ref());
            opts.merge_from(&mut input)?;
        }
        if opts.compute_size() > 0 {
            debug!("create options: {:?}", &opts);
        }

        let id = req.id();

        let stdio = match read_io(&bundle, req.id(), None).await {
            Ok(io) => Stdio::new(&io.stdin, &io.stdout, &io.stderr, io.terminal),
            Err(_) => Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal()),
        };

        // for qemu, the io path is pci address for virtio-serial
        // that needs to be converted to the serial file path
        let stdio = convert_stdio(&stdio).await?;

        let init = self.do_create(id, &stdio, &opts, &bundle).await?;
        let container = YoukiContainer {
            id: id.to_string(),
            bundle: bundle.to_string(),
            init,
            process_factory: YoukiExecFactory {
                bundle: bundle.to_string(),
                io_uid: opts.io_uid,
                io_gid: opts.io_gid,
            },
            processes: Default::default(),
        };
        Ok(container)
    }

    async fn cleanup(&self, _ns: &str, c: &YoukiContainer) -> containerd_shim::Result<()> {
        self.sandbox.lock().await.defer_storages(&c.id).await?;
        Ok(())
    }
}

impl YoukiFactory {
    pub fn new(sandbox: Arc<Mutex<SandboxResources>>) -> Self {
        Self { sandbox }
    }

    async fn do_create(
        &self,
        id: &str,
        stdio: &Stdio,
        opts: &Options,
        bundle: &str,
    ) -> Result<InitProcess> {
        // youki seems not support no_pivot_root and no_new_keyring option yet.

        let (socket, pio, _container_io) = if stdio.terminal {
            let s = ConsoleSocket::new().await?;
            (Some(s), None, None)
        } else {
            let (pio, container_io) = create_io(id, opts.io_uid, opts.io_gid, stdio)?;
            (None, Some(pio), Some(container_io))
        };
        let id_clone = id.to_string();
        let opts_clone = opts.clone();
        let bundle_clone = bundle.to_string();
        let socket_path = socket.as_ref().map(|p| p.path.clone());
        let resp = spawn_blocking(move || {
            let builder = ContainerBuilder::new(id_clone, SyscallType::default())
                .with_console_socket(socket_path)
                .with_root_path(PathBuf::from(YOUKI_DIR))
                .map_err(other_error!(e, "failed to set youki create root path"))?;
            // TODO: will uncomment this after youki-dev/youki#2961 is merged.
            // if let Some(f) = container_io {
            //     if let Some(p) = f.stdin {
            //         builder = builder.with_stdin(p);
            //     }
            //     if let Some(p) = f.stdout {
            //         builder = builder.with_stdout(p);
            //     }
            //     if let Some(p) = f.stderr {
            //         builder = builder.with_stderr(p);
            //     }
            // }

            builder
                .as_init(bundle_clone)
                .with_systemd(opts_clone.systemd_cgroup)
                .build()
                .map_err(other_error!(e, "failed to build youki container "))
        })
        .await
        .map_err(other_error!(e, "failed to wait container building "))?;

        match resp {
            Ok(mut c) => {
                let pid = if let Some(p) = c.pid() {
                    p.as_raw()
                } else {
                    if let Err(e) = c.delete(true) {
                        warn!("failed to cleanup container {}", e);
                    }
                    return Err(other!("failed to get pid of the youki container {}", id));
                };
                let mut init = InitProcess::new(
                    id,
                    stdio.clone(),
                    YoukiInitLifecycle::new(Arc::new(Mutex::new(c)), opts.clone(), bundle),
                );
                let exit_signal = init.lifecycle.exit_signal.clone();
                copy_io_or_console(&mut init, socket, pio, exit_signal).await?;
                init.pid = pid;
                Ok(init)
            }
            Err(e) => {
                if let Some(s) = socket {
                    s.clean().await;
                }
                Err(other!("failed to create container {}", e))
            }
        }
    }
}

pub struct YoukiExecFactory {
    bundle: String,
    io_uid: u32,
    io_gid: u32,
}

#[async_trait]
impl ProcessFactory<ExecProcess> for YoukiExecFactory {
    async fn create(&self, req: &ExecProcessRequest) -> Result<ExecProcess> {
        let p = get_spec_from_request(req)?;
        let stdio = match read_io(&self.bundle, req.id(), Some(req.exec_id())).await {
            // terminal is still determined from request
            Ok(io) => Stdio::new(&io.stdin, &io.stdout, &io.stderr, req.terminal()),
            Err(_) => Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal()),
        };
        let stdio = convert_stdio(&stdio).await?;
        Ok(ExecProcess {
            state: Status::CREATED,
            id: req.exec_id.to_string(),
            stdio,
            pid: 0,
            exit_code: 0,
            exited_at: None,
            wait_chan_tx: vec![],
            console: None,
            lifecycle: Arc::from(YoukiExecLifecycle {
                bundle: self.bundle.to_string(),
                container_id: req.id.to_string(),
                io_uid: self.io_uid,
                io_gid: self.io_gid,
                spec: p,
                exit_signal: Default::default(),
            }),
            stdin: Arc::new(Mutex::new(None)),
        })
    }
}

pub struct YoukiInitLifecycle {
    youki_container: Arc<Mutex<Container>>,
    exit_signal: Arc<ExitSignal>,
}

#[async_trait]
impl ProcessLifecycle<InitProcess> for YoukiInitLifecycle {
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        p.lifecycle
            .youki_container
            .lock()
            .await
            .start()
            .map_err(other_error!(e, "failed to start container "))?;
        p.state = Status::RUNNING;
        Ok(())
    }

    async fn kill(
        &self,
        p: &mut InitProcess,
        signal: u32,
        all: bool,
    ) -> containerd_shim::Result<()> {
        let signal = Signal::try_from(signal as i32)
            .map_err(other_error!(e, "failed to parse kill signal "))?;
        p.lifecycle
            .youki_container
            .lock()
            .await
            .kill(signal, all)
            .or_else(|e| {
                if let LibcontainerError::IncorrectStatus = e {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(other_error!(e, "failed to kill container "))?;
        Ok(())
    }

    async fn delete(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        p.lifecycle
            .youki_container
            .lock()
            .await
            .delete(true)
            .map_err(other_error!(e, "failed to delete container "))?;
        self.exit_signal.signal();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn update(&self, p: &mut InitProcess, resources: &LinuxResources) -> Result<()> {
        if p.pid <= 0 {
            return Err(other!(
                "failed to update resources because init process is {}",
                p.pid
            ));
        }
        containerd_shim::cgroup::update_resources(p.pid as u32, resources)
    }

    #[cfg(not(target_os = "linux"))]
    async fn update(&self, _p: &mut InitProcess, _resources: &LinuxResources) -> Result<()> {
        Err(Error::Unimplemented("update resource".to_string()))
    }

    #[cfg(target_os = "linux")]
    async fn stats(&self, p: &InitProcess) -> Result<Metrics> {
        if p.pid <= 0 {
            return Err(other!(
                "failed to collect metrics because init process is {}",
                p.pid
            ));
        }
        containerd_shim::cgroup::collect_metrics(p.pid as u32)
    }

    #[cfg(not(target_os = "linux"))]
    async fn stats(&self, _p: &InitProcess) -> Result<Metrics> {
        Err(Error::Unimplemented("process stats".to_string()))
    }

    async fn ps(&self, p: &InitProcess) -> Result<Vec<ProcessInfo>> {
        Ok(vec![ProcessInfo {
            pid: p.pid as u32,
            ..Default::default()
        }])
    }
}

impl YoukiInitLifecycle {
    pub fn new(youki_container: Arc<Mutex<Container>>, opts: Options, bundle: &str) -> Self {
        let work_dir = Path::new(bundle).join("work");
        let mut opts = opts;
        if opts.criu_path().is_empty() {
            opts.criu_path = work_dir.to_string_lossy().to_string();
        }
        Self {
            youki_container,
            exit_signal: Default::default(),
        }
    }
}

pub struct YoukiExecLifecycle {
    bundle: String,
    container_id: String,
    io_uid: u32,
    io_gid: u32,
    spec: Process,
    exit_signal: Arc<ExitSignal>,
}

#[async_trait]
impl ProcessLifecycle<ExecProcess> for YoukiExecLifecycle {
    async fn start(&self, p: &mut ExecProcess) -> containerd_shim::Result<()> {
        rescan_pci_bus().await?;
        let (socket, pio, _container_io) = if p.stdio.terminal {
            let s = ConsoleSocket::new().await?;
            (Some(s), None, None)
        } else {
            let (pio, container_io) = create_io(&p.id, self.io_uid, self.io_gid, &p.stdio)?;
            (None, Some(pio), Some(container_io))
        };

        let probe_path = format!("{}/{}-process.json", self.bundle, &p.id);
        let spec_str = serde_json::to_string(&self.spec)
            .map_err(other_error!(e, "failed to marshall exec spec to string"))?;
        tokio::fs::write(&probe_path, &spec_str)
            .await
            .map_err(other_error!(e, "failed to write spec to process.json"))?;

        let container_id = self.container_id.clone();
        let socket_path = socket.as_ref().map(|p| p.path.clone());
        let probe_path_clone = probe_path.clone();
        let exec_result = spawn_blocking(move || {
            let builder = ContainerBuilder::new(container_id, SyscallType::default())
                .with_root_path(PathBuf::from(YOUKI_DIR))
                .map_err(other_error!(e, "failed to set youki root path"))?
                .with_console_socket(socket_path);
            // TODO: will uncomment this after youki-dev/youki#2961 is merged.
            // if let Some(f) = container_io {
            //     if let Some(fd) = f.stdin {
            //         builder = builder.with_stdin(fd);
            //     }
            //     if let Some(fd) = f.stdout {
            //         builder = builder.with_stdout(fd);
            //     }
            //     if let Some(fd) = f.stderr {
            //         builder = builder.with_stderr(fd);
            //     }
            // }

            builder
                .as_tenant()
                .with_detach(true)
                .with_process(Some(&probe_path_clone))
                .build()
                .map_err(other_error!(
                    e,
                    "failed to exec process in youki container "
                ))
        })
        .await
        .map_err(other_error!(e, "failed to wait exec thread "))?;
        tokio::fs::remove_file(&probe_path)
            .await
            .unwrap_or_default();
        match exec_result {
            Ok(pid) => {
                copy_io_or_console(p, socket, pio, p.lifecycle.exit_signal.clone()).await?;
                p.pid = pid.as_raw();
                p.state = Status::RUNNING;
            }
            Err(e) => {
                if let Some(s) = socket {
                    s.clean().await;
                }
                return Err(other!("failed to start youki exec: {}", e));
            }
        }
        Ok(())
    }

    async fn kill(
        &self,
        p: &mut ExecProcess,
        signal: u32,
        _all: bool,
    ) -> containerd_shim::Result<()> {
        if p.pid <= 0 {
            Err(Error::FailedPreconditionError(
                "process not created".to_string(),
            ))
        } else if p.exited_at.is_some() {
            Err(Error::NotFoundError("process already finished".to_string()))
        } else {
            // TODO this is kill from nix crate, it is os specific, maybe have annotated with target os
            kill(
                Pid::from_raw(p.pid),
                nix::sys::signal::Signal::try_from(signal as i32).unwrap(),
            )
            .map_err(Into::into)
        }
    }

    async fn delete(&self, _p: &mut ExecProcess) -> containerd_shim::Result<()> {
        self.exit_signal.signal();
        Ok(())
    }

    async fn update(&self, _p: &mut ExecProcess, _resources: &LinuxResources) -> Result<()> {
        Err(Error::Unimplemented("exec update".to_string()))
    }

    async fn stats(&self, _p: &ExecProcess) -> Result<Metrics> {
        Err(Error::Unimplemented("exec stats".to_string()))
    }

    async fn ps(&self, _p: &ExecProcess) -> Result<Vec<ProcessInfo>> {
        Err(Error::Unimplemented("exec ps".to_string()))
    }
}

pub fn create_io(
    _id: &str,
    io_uid: u32,
    io_gid: u32,
    stdio: &Stdio,
) -> containerd_shim::Result<(ProcessIO, ContainerPipeIo)> {
    if stdio.is_null() {
        let nio = NullIo::new().map_err(io_error!(e, "new Null Io"))?;
        let pio = ProcessIO {
            io: Some(Arc::new(nio)),
            copy: false,
        };
        return Ok((pio, ContainerPipeIo::default()));
    }
    let stdout = stdio.stdout.as_str();
    let scheme_path = stdout.trim().split("://").collect::<Vec<&str>>();
    let _scheme = if scheme_path.len() <= 1 {
        // no scheme specified
        // default schema to fifo
        "fifo"
    } else {
        scheme_path[0]
    };

    let mut pio = ProcessIO {
        io: None,
        copy: false,
    };

    let opt = IOOption {
        open_stdin: !stdio.stdin.is_empty(),
        open_stdout: !stdio.stdout.is_empty(),
        open_stderr: !stdio.stderr.is_empty(),
    };
    let (io, container_io) =
        PipedIo::new(io_uid, io_gid, &opt).map_err(io_error!(e, "new PipedIo"))?;
    pio.io = Some(Arc::new(io));
    pio.copy = true;
    Ok((pio, container_io))
}

impl Io for PipedIo {
    fn stdin(&self) -> Option<Box<dyn AsyncWrite + Send + Sync + Unpin>> {
        self.stdin.and_then(|pipe| {
            tokio_pipe::PipeWrite::from_raw_fd_checked(pipe)
                .map(|x| Box::new(x) as Box<dyn AsyncWrite + Send + Sync + Unpin>)
                .ok()
        })
    }

    fn stdout(&self) -> Option<Box<dyn AsyncRead + Send + Sync + Unpin>> {
        self.stdout.and_then(|pipe| {
            tokio_pipe::PipeRead::from_raw_fd_checked(pipe)
                .map(|x| Box::new(x) as Box<dyn AsyncRead + Send + Sync + Unpin>)
                .ok()
        })
    }

    fn stderr(&self) -> Option<Box<dyn AsyncRead + Send + Sync + Unpin>> {
        self.stderr.and_then(|pipe| {
            tokio_pipe::PipeRead::from_raw_fd_checked(pipe)
                .map(|x| Box::new(x) as Box<dyn AsyncRead + Send + Sync + Unpin>)
                .ok()
        })
    }

    // Note that this internally use [`std::fs::File`]'s `try_clone()`.
    // Thus, the files passed to commands will be not closed after command exit.
    fn set(&self, _cmd: &mut Command) -> std::io::Result<()> {
        Ok(())
    }

    fn close_after_start(&self) {}
}

/// Struct to represent a pipe that can be used to transfer stdio inputs and outputs.
///
/// With this Io driver, methods of [crate::Runc] may capture the output/error messages.
/// When one side of the pipe is closed, the state will be represented with [`None`].
#[derive(Debug)]
pub struct Pipe {
    rd: RawFd,
    wr: RawFd,
}

#[derive(Debug, Default)]
pub struct PipedIo {
    stdin: Option<RawFd>,
    stdout: Option<RawFd>,
    stderr: Option<RawFd>,
}

#[derive(Debug, Default)]
pub struct ContainerPipeIo {
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
}

impl Pipe {
    fn new(uid: u32, gid: u32, stdin: bool) -> std::io::Result<Self> {
        let (rd, wr) = os_pipe::pipe()?;
        if stdin {
            fchown(&rd, Some(uid), Some(gid))?;
        } else {
            fchown(&wr, Some(uid), Some(gid))?;
        }
        Ok(Self {
            rd: rd.into_raw_fd(),
            wr: wr.into_raw_fd(),
        })
    }
}

impl PipedIo {
    pub fn new(uid: u32, gid: u32, opts: &IOOption) -> std::io::Result<(Self, ContainerPipeIo)> {
        let mut res = Self::default();
        let mut container_pipes = ContainerPipeIo::default();
        if opts.open_stdin {
            let pipe = Self::create_pipe(uid, gid, true)?;
            res.stdin = Some(pipe.wr);
            container_pipes.stdin = Some(unsafe { OwnedFd::from_raw_fd(pipe.rd) });
        }
        if opts.open_stdout {
            let pipe = Self::create_pipe(uid, gid, false)?;
            res.stdout = Some(pipe.rd);
            container_pipes.stdout = Some(unsafe { OwnedFd::from_raw_fd(pipe.wr) });
        }
        if opts.open_stderr {
            let pipe = Self::create_pipe(uid, gid, false)?;
            res.stderr = Some(pipe.rd);
            container_pipes.stderr = Some(unsafe { OwnedFd::from_raw_fd(pipe.wr) });
        }
        Ok((res, container_pipes))
    }

    fn create_pipe(uid: u32, gid: u32, stdin: bool) -> std::io::Result<Pipe> {
        Pipe::new(uid, gid, stdin)
    }
}

fn get_spec_from_request(
    req: &ExecProcessRequest,
) -> containerd_shim::Result<oci_spec::runtime::Process> {
    if let Some(val) = req.spec.as_ref() {
        let mut p = serde_json::from_slice::<oci_spec::runtime::Process>(&val.value)?;
        p.set_terminal(Some(req.terminal));
        Ok(p)
    } else {
        Err(Error::InvalidArgument("no spec in request".to_string()))
    }
}
