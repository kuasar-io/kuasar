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
    convert::TryFrom,
    os::unix::prelude::ExitStatusExt,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};

use async_trait::async_trait;
use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest, Status},
    asynchronous::{
        console::ConsoleSocket,
        container::{ContainerFactory, ContainerTemplate, ProcessFactory},
        monitor::{monitor_subscribe, monitor_unsubscribe},
        processes::{ProcessLifecycle, ProcessTemplate},
        util::read_file_to_str,
    },
    error::{Error, Result},
    io::Stdio,
    monitor::Topic,
    other, other_error,
    protos::{
        cgroups::metrics::Metrics,
        protobuf::{CodedInputStream, Message},
        shim::oci::Options,
        types::task::ProcessInfo,
    },
    util::read_spec,
    ExitSignal,
};
use libcontainer::{
    container::{builder::ContainerBuilder, Container},
    signal::Signal,
    syscall::syscall::SyscallType,
};
use log::{debug, error};
use nix::{sys::signalfd::signal::kill, unistd::Pid};
use oci_spec::runtime::{LinuxResources, Process, Spec};
use runc::Spawner;
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
};
use vmm_common::{mount::get_mount_type, storage::Storage, KUASAR_STATE_DIR, YOUKI_DIR};

use crate::{
    device::rescan_pci_bus,
    io::{convert_stdio, copy_io_or_console, create_io},
    sandbox::SandboxResources,
    util::{read_io, read_storages, wait_pid},
};

#[allow(dead_code)]
pub const GROUP_LABELS: [&str; 2] = [
    "io.containerd.runc.v2.group",
    "io.kubernetes.cri.sandbox-id",
];
pub const INIT_PID_FILE: &str = "init.pid";

const STORAGE_ANNOTATION: &str = "io.kuasar.storages";

pub type ExecProcess = ProcessTemplate<KuasarExecLifecycle>;
pub type InitProcess = ProcessTemplate<KuasarInitLifecycle>;

pub type KuasarContainer = ContainerTemplate<InitProcess, ExecProcess, KuasarExecFactory>;

#[derive(Clone)]
pub(crate) struct KuasarFactory {
    sandbox: Arc<Mutex<SandboxResources>>,
}

pub struct KuasarExecFactory {
    bundle: String,
    io_uid: u32,
    io_gid: u32,
}

pub struct KuasarExecLifecycle {
    bundle: String,
    container_id: String,
    io_uid: u32,
    io_gid: u32,
    spec: Process,
    exit_signal: Arc<ExitSignal>,
}

pub struct KuasarInitLifecycle {
    opts: Options,
    bundle: String,
    exit_signal: Arc<ExitSignal>,
}

#[derive(Deserialize)]
pub struct Log {
    pub level: String,
    pub msg: String,
}

#[async_trait]
impl ContainerFactory<KuasarContainer> for KuasarFactory {
    async fn create(
        &self,
        _ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<KuasarContainer> {
        rescan_pci_bus().await?;
        let bundle = format!("{}/{}", KUASAR_STATE_DIR, req.id);
        let spec: Spec = read_spec(&bundle).await?;
        let annotations = spec.annotations().clone().unwrap_or_default();
        let storages = if let Some(storage_str) = annotations.get(STORAGE_ANNOTATION) {
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

        // As the rootfs is already mounted when handling the storage, the root in spec is one of the
        // storage mount point. so no need to mount rootfs anymore
        let id = req.id();

        let stdio = match read_io(&bundle, req.id(), None).await {
            Ok(io) => Stdio::new(&io.stdin, &io.stdout, &io.stderr, io.terminal),
            Err(_) => Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal()),
        };

        // for qemu, the io path is pci address for virtio-serial
        // that needs to be converted to the serial file path
        let stdio = convert_stdio(&stdio).await?;

        let mut init = InitProcess::new(id, stdio, KuasarInitLifecycle::new(opts.clone(), &bundle));

        self.do_create(&mut init).await?;
        let container = KuasarContainer {
            id: id.to_string(),
            bundle: bundle.to_string(),
            init,
            process_factory: KuasarExecFactory {
                bundle: bundle.to_string(),
                io_uid: opts.io_uid,
                io_gid: opts.io_gid,
            },
            processes: Default::default(),
        };
        Ok(container)
    }

    async fn cleanup(&self, _ns: &str, c: &KuasarContainer) -> containerd_shim::Result<()> {
        self.sandbox.lock().await.defer_storages(&c.id).await?;
        Ok(())
    }
}

impl KuasarFactory {
    pub fn new(sandbox: Arc<Mutex<SandboxResources>>) -> Self {
        Self { sandbox }
    }

    async fn do_create(&self, init: &mut InitProcess) -> Result<()> {
        let id = init.id.to_string();
        let stdio = &init.stdio;
        let opts = &init.lifecycle.opts;
        let bundle = &init.lifecycle.bundle;
        let pid_path = Path::new(bundle).join(INIT_PID_FILE);
        let mut _no_pivot_root = opts.no_pivot_root;
        // pivot_root could not work with initramfs
        // TODO youki not support no_pivot_root yet
        match get_mount_type("/") {
            Ok(m_type) => {
                if m_type == *"rootfs" {
                    _no_pivot_root = true;
                }
            }
            Err(e) => debug!("get mount type failed {}", e),
        };
        let mut socket_path = PathBuf::new();
        let (socket, pio) = if stdio.terminal {
            let s = ConsoleSocket::new().await?;
            socket_path = s.path.to_owned();
            (Some(s), None)
        } else {
            let pio = create_io(&id, opts.io_uid, opts.io_gid, stdio)?;
            (None, Some(pio))
        };
        let resp = ContainerBuilder::new(id.to_string(), SyscallType::default())
            .with_pid_file(Some(pid_path.clone()))
            .map_err(other_error!(e, "failed to set youki create pid file"))?
            .with_console_socket(Some(socket_path))
            .with_root_path(PathBuf::from(YOUKI_DIR))
            .map_err(other_error!(e, "failed to set youki create root path"))?
            .as_init(bundle)
            .with_systemd(false)
            .with_detach(false)
            .build();
        if let Err(e) = resp {
            if let Some(s) = socket {
                s.clean().await;
            }
            return Err(other!(
                "youki create container {} failed {}",
                id.to_string(),
                e
            ));
        }
        copy_io_or_console(init, socket, pio, init.lifecycle.exit_signal.clone()).await?;
        let pid = read_file_to_str(pid_path).await?.parse::<i32>()?;
        init.pid = pid;
        Ok(())
    }
}

#[async_trait]
impl ProcessFactory<ExecProcess> for KuasarExecFactory {
    async fn create(&self, req: &ExecProcessRequest) -> Result<ExecProcess> {
        let p = get_spec_from_request(req)?;
        let stdio = match read_io(&self.bundle, req.id(), Some(req.exec_id())).await {
            Ok(io) => Stdio::new(&io.stdin, &io.stdout, &io.stderr, io.terminal),
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
            lifecycle: Arc::from(KuasarExecLifecycle {
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

#[async_trait]
impl ProcessLifecycle<InitProcess> for KuasarInitLifecycle {
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        let mut container = Container::load(PathBuf::from(YOUKI_DIR).join(p.id.as_str()))
            .map_err(other_error!(e, "youki load container error"))?;

        container
            .start()
            .map_err(other_error!(e, "youki start container error"))?;
        p.state = Status::RUNNING;
        Ok(())
    }

    async fn kill(
        &self,
        p: &mut InitProcess,
        signal: u32,
        all: bool,
    ) -> containerd_shim::Result<()> {
        let mut container = Container::load(PathBuf::from(YOUKI_DIR).join(p.id.as_str()))
            .map_err(other_error!(e, "youki load container error"))?;

        let kill_signal: Signal =
            signal.to_string().as_str().try_into().map_err(|e| {
                Error::Other(format!("kill signal {} transfer error: {}", signal, e))
            })?;

        container
            .kill(kill_signal, all)
            .map_err(other_error!(e, "youki kill container error"))
            .map_err(|e| check_kill_error(e.to_string()))
    }

    async fn delete(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        let mut container = Container::load(PathBuf::from(YOUKI_DIR).join(p.id.as_str()))
            .map_err(other_error!(e, "youki load container error"))?;

        container
            .delete(true)
            .or_else(|e| {
                if !e.to_string().to_lowercase().contains("does not exist") {
                    Err(e)
                } else {
                    Ok(())
                }
            })
            .map_err(other_error!(e, "youki delete container error"))?;
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
        let container = Container::load(PathBuf::from(YOUKI_DIR).join(p.id.as_str()))
            .map_err(other_error!(e, "youki load container error"))?;

        Ok(container
            .pid()
            .iter()
            .map(|&x| ProcessInfo {
                pid: x.as_raw() as u32,
                ..Default::default()
            })
            .collect())
    }
}

impl KuasarInitLifecycle {
    pub fn new(opts: Options, bundle: &str) -> Self {
        let work_dir = Path::new(bundle).join("work");
        let mut opts = opts;
        if opts.criu_path().is_empty() {
            opts.criu_path = work_dir.to_string_lossy().to_string();
        }
        Self {
            opts,
            bundle: bundle.to_string(),
            exit_signal: Default::default(),
        }
    }
}

#[async_trait]
impl ProcessLifecycle<ExecProcess> for KuasarExecLifecycle {
    async fn start(&self, p: &mut ExecProcess) -> containerd_shim::Result<()> {
        rescan_pci_bus().await?;
        let pid_path = Path::new(self.bundle.as_str()).join(format!("{}.pid", &p.id));
        let mut socket_path = PathBuf::new();
        let (socket, pio) = if p.stdio.terminal {
            let s = ConsoleSocket::new().await?;
            socket_path = s.path.to_owned();
            (Some(s), None)
        } else {
            let pio = create_io(&p.id, self.io_uid, self.io_gid, &p.stdio)?;
            (None, Some(pio))
        };

        let probe_path = format!("{}/{}-process.json", self.bundle, &p.id);
        let spec_str = serde_json::to_string(&self.spec)
            .map_err(other_error!(e, "failed to marshall exec spec to string"))?;
        tokio::fs::write(&probe_path, &spec_str)
            .await
            .map_err(other_error!(e, "failed to write spec to process.json"))?;

        //TODO  checkpoint support
        let exec_result = ContainerBuilder::new(self.container_id.clone(), SyscallType::default())
            .with_root_path(PathBuf::from(YOUKI_DIR))
            .map_err(other_error!(e, "failed to set youki root path"))?
            .with_console_socket(Some(socket_path))
            .with_pid_file(Some(pid_path.clone()))
            .map_err(other_error!(e, "failed to set process pid file"))?
            .as_tenant()
            .with_detach(true)
            .with_process(Some(&probe_path))
            .build();
        if let Err(e) = exec_result {
            if let Some(s) = socket {
                s.clean().await;
            }
            let _ = tokio::fs::remove_file(&probe_path).await;
            return Err(other!(
                "youki exec container {} failed {}",
                self.container_id,
                e
            ));
        }
        let _ = tokio::fs::remove_file(&probe_path).await;

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

#[derive(Default, Debug)]
pub struct ShimExecutor {}

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

pub fn check_kill_error(emsg: String) -> Error {
    let emsg = emsg.to_lowercase();
    if emsg.contains("process already finished")
        || emsg.contains("container not running")
        || emsg.contains("no such process")
    {
        Error::NotFoundError("process already finished".to_string())
    } else if emsg.contains("does not exist") {
        Error::NotFoundError("no such container".to_string())
    } else {
        other!("unknown error after kill {}", emsg)
    }
}
