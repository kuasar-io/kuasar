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
    os::unix::{
        io::{AsRawFd, FromRawFd, RawFd},
        prelude::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};

use async_trait::async_trait;
use containerd_sandbox::spec::{get_sandbox_id, JsonSpec};
use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest, Options, Status},
    asynchronous::{
        console::ConsoleSocket,
        container::{ContainerFactory, ContainerTemplate, ProcessFactory},
        monitor::{monitor_subscribe, monitor_unsubscribe, Subscription},
        processes::{ProcessLifecycle, ProcessTemplate},
    },
    io::Stdio,
    io_error,
    monitor::{ExitEvent, Subject, Topic},
    other, other_error,
    protos::{
        api::ProcessInfo,
        cgroups::metrics::Metrics,
        protobuf::{CodedInputStream, Message},
    },
    util::{asyncify, mkdir, mount_rootfs, read_file_to_str, write_options, write_runtime},
    Console, Error, ExitSignal, Result,
};
use containerd_shim::{
    asynchronous::util::write_str_to_file,
    util::{read_spec, CONFIG_FILE_NAME},
};
use log::{debug, error};
use nix::{sys::signal::kill, unistd::Pid};
use oci_spec::runtime::{LinuxResources, Process};
use runc::{Command, Runc, Spawner};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
};

use crate::common::{
    check_kill_error, create_io, create_runc, get_spec_from_request, receive_socket, CreateConfig,
    ProcessIO, ShimExecutor, INIT_PID_FILE,
};

pub type ExecProcess = ProcessTemplate<RuncExecLifecycle>;
pub type InitProcess = ProcessTemplate<RuncInitLifecycle>;

pub type RuncContainer = ContainerTemplate<InitProcess, ExecProcess, RuncExecFactory>;

#[derive(Clone)]
pub(crate) struct RuncFactory {
    sandbox_parent_dir: String,
}

#[derive(Serialize, Deserialize)]
pub struct SerializableStdio {
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub terminal: bool,
}

impl From<SerializableStdio> for Stdio {
    fn from(value: SerializableStdio) -> Self {
        Self {
            stdin: value.stdin,
            stdout: value.stdout,
            stderr: value.stderr,
            terminal: value.terminal,
        }
    }
}

impl From<Stdio> for SerializableStdio {
    fn from(value: Stdio) -> Self {
        Self {
            stdin: value.stdin,
            stdout: value.stdout,
            stderr: value.stderr,
            terminal: value.terminal,
        }
    }
}

#[async_trait]
impl ContainerFactory<RuncContainer> for RuncFactory {
    async fn create(
        &self,
        ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<RuncContainer> {
        let bundle = req.bundle();
        let mut opts = Options::new();
        if let Some(any) = req.options.as_ref() {
            let mut input = CodedInputStream::from_bytes(any.value.as_ref());
            opts.merge_from(&mut input)?;
        }
        if opts.compute_size() > 0 {
            debug!("create options: {:?}", &opts);
        }
        let runtime = opts.binary_name.as_str();
        write_options(bundle, &opts).await?;
        write_runtime(bundle, runtime).await?;

        self.rewrite_spec(bundle).await?;

        let rootfs_vec = req.rootfs().to_vec();
        let rootfs = if !rootfs_vec.is_empty() {
            let tmp_rootfs = Path::new(bundle).join("rootfs");
            mkdir(&tmp_rootfs, 0o711).await?;
            tmp_rootfs
        } else {
            PathBuf::new()
        };

        for m in rootfs_vec {
            mount_rootfs(&m, rootfs.as_path()).await?
        }

        let runc = create_runc(
            runtime,
            ns,
            bundle,
            &opts,
            Some(Arc::new(ShimExecutor::default())),
        )?;

        let id = req.id();
        let stdio = Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal());
        write_stdio(bundle, &stdio).await?;

        let mut init = InitProcess::new(
            id,
            stdio,
            RuncInitLifecycle::new(runc.clone(), opts.clone(), bundle),
        );

        let config = CreateConfig::default();
        self.do_create(&mut init, config).await?;
        let container = RuncContainer {
            id: id.to_string(),
            bundle: bundle.to_string(),
            init,
            process_factory: RuncExecFactory {
                runtime: runc,
                bundle: bundle.to_string(),
                io_uid: opts.io_uid,
                io_gid: opts.io_gid,
            },
            processes: Default::default(),
        };
        Ok(container)
    }

    async fn cleanup(&self, _ns: &str, _c: &RuncContainer) -> containerd_shim::Result<()> {
        Ok(())
    }
}

impl RuncFactory {
    pub fn new(sandbox_parent_dir: &str) -> Self {
        Self {
            sandbox_parent_dir: sandbox_parent_dir.to_string(),
        }
    }

    async fn do_create(&self, init: &mut InitProcess, _config: CreateConfig) -> Result<()> {
        let id = init.id.to_string();
        let stdio = &init.stdio;
        let opts = &init.lifecycle.opts;
        let bundle = &init.lifecycle.bundle;
        let pid_path = Path::new(bundle).join(INIT_PID_FILE);
        let mut create_opts = runc::options::CreateOpts::new()
            .pid_file(&pid_path)
            .no_pivot(opts.no_pivot_root)
            .no_new_keyring(opts.no_new_keyring)
            .detach(false);
        let (socket, pio) = if stdio.terminal {
            let s = ConsoleSocket::new().await?;
            create_opts.console_socket = Some(s.path.to_owned());
            (Some(s), None)
        } else {
            let pio = create_io(&id, opts.io_uid, opts.io_gid, stdio)?;
            create_opts.io = pio.io.as_ref().cloned();
            (None, Some(pio))
        };

        let resp = init
            .lifecycle
            .runtime
            .create(&id, bundle, Some(&create_opts))
            .await;
        if let Err(e) = resp {
            if let Some(s) = socket {
                s.clean().await;
            }
            let runtime_e = runtime_error(e, bundle).await;
            return Err(runtime_e);
        }
        copy_io_or_console(init, socket, pio, init.lifecycle.exit_signal.clone()).await?;
        let pid = read_file_to_str(pid_path).await?.parse::<i32>()?;
        init.pid = pid;
        Ok(())
    }

    async fn rewrite_spec(&self, bundle: &str) -> Result<()> {
        let mut spec: JsonSpec = read_spec(bundle).await?;
        let sandbox_id = if let Some(id) = get_sandbox_id(&spec.annotations) {
            id
        } else {
            return Ok(());
        };

        if let Some(l) = &mut spec.linux {
            for ns in &mut l.namespaces {
                if ns.r#type == "uts" && ns.path == "/proc/0/ns/uts" {
                    ns.path = format!("{}/{}/uts", self.sandbox_parent_dir, sandbox_id);
                }
                if ns.r#type == "ipc" && ns.path == "/proc/0/ns/ipc" {
                    ns.path = format!("{}/{}/ipc", self.sandbox_parent_dir, sandbox_id);
                }
                if ns.r#type == "network" && ns.path == "/proc/0/ns/net" {
                    ns.path = format!("{}/{}/net", self.sandbox_parent_dir, sandbox_id);
                }
            }
        }
        let config_file_path = format!("{}/{}", bundle, CONFIG_FILE_NAME);
        let spec_content_new = serde_json::to_string(&spec)
            .map_err(other_error!(e, "failed to marshal spec to string"))?;
        write_str_to_file(&config_file_path, &spec_content_new).await?;
        Ok(())
    }
}

pub struct RuncExecFactory {
    runtime: Runc,
    bundle: String,
    io_uid: u32,
    io_gid: u32,
}

#[async_trait]
impl ProcessFactory<ExecProcess> for RuncExecFactory {
    async fn create(&self, req: &ExecProcessRequest) -> Result<ExecProcess> {
        let p = get_spec_from_request(req)?;
        Ok(ExecProcess {
            state: Status::CREATED,
            id: req.exec_id.to_string(),
            stdio: Stdio {
                stdin: req.stdin.to_string(),
                stdout: req.stdout.to_string(),
                stderr: req.stderr.to_string(),
                terminal: req.terminal,
            },
            pid: 0,
            exit_code: 0,
            exited_at: None,
            wait_chan_tx: vec![],
            console: None,
            lifecycle: Arc::from(RuncExecLifecycle {
                runtime: self.runtime.clone(),
                bundle: self.bundle.to_string(),
                container_id: req.id.to_string(),
                io_uid: self.io_uid,
                io_gid: self.io_gid,
                spec: p,
                exit_signal: Default::default(),
            }),
        })
    }
}

pub struct RuncInitLifecycle {
    runtime: Runc,
    opts: Options,
    bundle: String,
    exit_signal: Arc<ExitSignal>,
}

#[async_trait]
impl ProcessLifecycle<InitProcess> for RuncInitLifecycle {
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        self.runtime
            .start(p.id.as_str())
            .await
            .map_err(other_error!(e, "failed start"))?;
        p.state = Status::RUNNING;
        Ok(())
    }

    async fn kill(
        &self,
        p: &mut InitProcess,
        signal: u32,
        all: bool,
    ) -> containerd_shim::Result<()> {
        self.runtime
            .kill(
                p.id.as_str(),
                signal,
                Some(&runc::options::KillOpts { all }),
            )
            .await
            .map_err(|e| check_kill_error(e.to_string()))
    }

    async fn delete(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        self.runtime
            .delete(
                p.id.as_str(),
                Some(&runc::options::DeleteOpts { force: true }),
            )
            .await
            .or_else(|e| {
                if !e.to_string().to_lowercase().contains("does not exist") {
                    Err(e)
                } else {
                    Ok(())
                }
            })
            .map_err(other_error!(e, "failed delete"))?;
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
        let pids = self
            .runtime
            .ps(&p.id)
            .await
            .map_err(other_error!(e, "failed to execute runc ps"))?;
        Ok(pids
            .iter()
            .map(|&x| ProcessInfo {
                pid: x as u32,
                ..Default::default()
            })
            .collect())
    }
}

impl RuncInitLifecycle {
    pub fn new(runtime: Runc, opts: Options, bundle: &str) -> Self {
        let work_dir = Path::new(bundle).join("work");
        let mut opts = opts;
        if opts.criu_path().is_empty() {
            opts.criu_path = work_dir.to_string_lossy().to_string();
        }
        Self {
            runtime,
            opts,
            bundle: bundle.to_string(),
            exit_signal: Default::default(),
        }
    }
}

pub struct RuncExecLifecycle {
    runtime: Runc,
    bundle: String,
    container_id: String,
    io_uid: u32,
    io_gid: u32,
    spec: Process,
    exit_signal: Arc<ExitSignal>,
}

#[async_trait]
impl ProcessLifecycle<ExecProcess> for RuncExecLifecycle {
    async fn start(&self, p: &mut ExecProcess) -> containerd_shim::Result<()> {
        let pid_path = Path::new(self.bundle.as_str()).join(format!("{}.pid", &p.id));
        let mut exec_opts = runc::options::ExecOpts {
            io: None,
            pid_file: Some(pid_path.to_owned()),
            console_socket: None,
            detach: true,
        };
        let (socket, pio) = if p.stdio.terminal {
            let s = ConsoleSocket::new().await?;
            exec_opts.console_socket = Some(s.path.to_owned());
            (Some(s), None)
        } else {
            let pio = create_io(&p.id, self.io_uid, self.io_gid, &p.stdio)?;
            exec_opts.io = pio.io.as_ref().cloned();
            (None, Some(pio))
        };
        //TODO  checkpoint support
        let exec_result = self
            .runtime
            .exec(&self.container_id, &self.spec, Some(&exec_opts))
            .await;
        if let Err(e) = exec_result {
            if let Some(s) = socket {
                s.clean().await;
            }
            return Err(other!("failed to start runc exec: {}", e));
        }
        copy_io_or_console(p, socket, pio, p.lifecycle.exit_signal.clone()).await?;
        let pid = read_file_to_str(pid_path).await?.parse::<i32>()?;
        p.pid = pid;
        p.state = Status::RUNNING;
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

async fn copy_console(
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
        let console_stdin = f
            .try_clone()
            .await
            .map_err(io_error!(e, "failed to clone console file"))?;
        let stdin_fut = async {
            OpenOptions::new()
                .read(true)
                .open(stdio.stdin.as_str())
                .await
        };
        let stdin_w_fut = async {
            OpenOptions::new()
                .write(true)
                .open(stdio.stdin.as_str())
                .await
        };
        let (stdin, stdin_w) =
            tokio::try_join!(stdin_fut, stdin_w_fut).map_err(io_error!(e, "open stdin"))?;
        spawn_copy(
            stdin,
            console_stdin,
            exit_signal.clone(),
            Some(move || {
                drop(stdin_w);
            }),
        );
    }

    if !stdio.stdout.is_empty() {
        let console_stdout = f
            .try_clone()
            .await
            .map_err(io_error!(e, "failed to clone console file"))?;
        debug!("copy_console: pipe stdout from console");
        let stdout = OpenOptions::new()
            .write(true)
            .open(stdio.stdout.as_str())
            .await
            .map_err(io_error!(e, "open stdout"))?;
        // open a read to make sure even if the read end of containerd shutdown,
        // copy still continue until the restart of containerd succeed
        let stdout_r = OpenOptions::new()
            .read(true)
            .open(stdio.stdout.as_str())
            .await
            .map_err(io_error!(e, "open stdout for read"))?;
        spawn_copy(
            console_stdout,
            stdout,
            exit_signal,
            Some(move || {
                drop(stdout_r);
            }),
        );
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
                let stdin = OpenOptions::new()
                    .read(true)
                    .open(stdio.stdin.as_str())
                    .await
                    .map_err(io_error!(e, "open stdin"))?;
                spawn_copy(stdin, w, exit_signal.clone(), None::<fn()>);
            }
        }

        if let Some(r) = io.stdout() {
            debug!("copy_io: pipe stdout from to {}", stdio.stdout.as_str());
            if !stdio.stdout.is_empty() {
                let stdout = OpenOptions::new()
                    .write(true)
                    .open(stdio.stdout.as_str())
                    .await
                    .map_err(io_error!(e, "open stdout"))?;
                // open a read to make sure even if the read end of containerd shutdown,
                // copy still continue until the restart of containerd succeed
                let stdout_r = OpenOptions::new()
                    .read(true)
                    .open(stdio.stdout.as_str())
                    .await
                    .map_err(io_error!(e, "open stdout for read"))?;
                spawn_copy(
                    r,
                    stdout,
                    exit_signal.clone(),
                    Some(move || {
                        drop(stdout_r);
                    }),
                );
            }
        }

        if let Some(r) = io.stderr() {
            if !stdio.stderr.is_empty() {
                debug!("copy_io: pipe stderr from to {}", stdio.stderr.as_str());
                let stderr = OpenOptions::new()
                    .write(true)
                    .open(stdio.stderr.as_str())
                    .await
                    .map_err(io_error!(e, "open stderr"))?;
                // open a read to make sure even if the read end of containerd shutdown,
                // copy still continue until the restart of containerd succeed
                let stderr_r = OpenOptions::new()
                    .read(true)
                    .open(stdio.stderr.as_str())
                    .await
                    .map_err(io_error!(e, "open stderr for read"))?;
                spawn_copy(
                    r,
                    stderr,
                    exit_signal,
                    Some(move || {
                        drop(stderr_r);
                    }),
                );
            }
        }
    }

    Ok(())
}

fn spawn_copy<R, W, F>(from: R, to: W, exit_signal: Arc<ExitSignal>, on_close: Option<F>)
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    let mut src = from;
    let mut dst = to;
    tokio::spawn(async move {
        tokio::select! {
            _ = exit_signal.wait() => {
                debug!("container exit, copy task should exit too");
            },
            res = tokio::io::copy(&mut src, &mut dst) => {
               if let Err(e) = res {
                    error!("copy io failed {}", e);
                }
            }
        }
        if let Some(f) = on_close {
            f();
        }
    });
}

async fn copy_io_or_console<P>(
    p: &mut ProcessTemplate<P>,
    socket: Option<ConsoleSocket>,
    pio: Option<ProcessIO>,
    exit_signal: Arc<ExitSignal>,
) -> Result<()> {
    if p.stdio.terminal {
        if let Some(console_socket) = socket {
            let console_result = copy_console(&console_socket, &p.stdio, exit_signal).await;
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

#[async_trait]
impl Spawner for ShimExecutor {
    async fn execute(
        &self,
        cmd: Command,
        after_start: Box<dyn Fn() + Send>,
        wait_output: bool,
    ) -> runc::Result<(ExitStatus, u32, String, String)> {
        let mut cmd = cmd;
        let subscription = monitor_subscribe(Topic::Pid)
            .await
            .map_err(|e| runc::error::Error::Other(Box::new(e)))?;
        let sid = subscription.id;
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                monitor_unsubscribe(sid).await.unwrap_or_default();
                return Err(runc::error::Error::ProcessSpawnFailed(e));
            }
        };
        after_start();
        let pid = child.id().unwrap();
        let (stdout, stderr, exit_code) = if wait_output {
            tokio::join!(
                read_std(child.stdout),
                read_std(child.stderr),
                wait_pid(pid as i32, subscription)
            )
        } else {
            (
                "".to_string(),
                "".to_string(),
                wait_pid(pid as i32, subscription).await,
            )
        };
        let status = ExitStatus::from_raw(exit_code);
        monitor_unsubscribe(sid).await.unwrap_or_default();
        Ok((status, pid, stdout, stderr))
    }
}

async fn read_std<T>(std: Option<T>) -> String
where
    T: AsyncRead + Unpin,
{
    let mut std = std;
    if let Some(mut std) = std.take() {
        let mut out = String::new();
        std.read_to_string(&mut out).await.unwrap_or_else(|e| {
            error!("failed to read stdout {}", e);
            0
        });
        return out;
    }
    "".to_string()
}

async fn wait_pid(pid: i32, s: Subscription) -> i32 {
    let mut s = s;
    loop {
        if let Some(ExitEvent {
            subject: Subject::Pid(epid),
            exit_code: code,
        }) = s.rx.recv().await
        {
            if pid == epid {
                monitor_unsubscribe(s.id).await.unwrap_or_default();
                return code;
            }
        }
    }
}

#[derive(Deserialize)]
struct Log {
    level: String,
    msg: String,
}

// runtime_error will read the OCI runtime logfile collecting runtime error
async fn runtime_error(e: runc::error::Error, bundle: &str) -> Error {
    let mut msg = String::new();
    if let Ok(file) = File::open(Path::new(bundle).join("log.json")).await {
        let mut lines = BufReader::new(file).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(log) = serde_json::from_str::<Log>(&line) {
                // according to golang shim, take the last runtime error as error
                if log.level == "error" {
                    msg = log.msg.trim().to_string();
                }
            }
        }
    }
    if !msg.is_empty() {
        other!("{}", msg)
    } else {
        other!("unable to retrieve OCI runtime error {:?}", e)
    }
}

async fn write_stdio(bundle: &str, stdio: &Stdio) -> Result<()> {
    let serializable_stdio: SerializableStdio = stdio.clone().into();
    let stdio_str = serde_json::to_string(&serializable_stdio)
        .map_err(other_error!(e, "failed to serialize stdio"))?;
    write_str_to_file(Path::new(bundle).join("stdio"), stdio_str).await
}
