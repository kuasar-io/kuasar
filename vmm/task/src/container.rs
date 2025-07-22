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
    convert::TryFrom, io::SeekFrom, os::unix::prelude::ExitStatusExt, path::Path,
    process::ExitStatus, sync::Arc,
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
use log::{debug, error};
use nix::{sys::signalfd::signal::kill, unistd::Pid};
use oci_spec::runtime::{LinuxResources, Process, Spec};
use runc::{options::GlobalOpts, Runc, Spawner};
use serde::Deserialize;
use tokio::{
    fs::{remove_file, File},
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeekExt, BufReader},
    process::Command,
    sync::Mutex,
};
use tracing::instrument;
use vmm_common::{
    mount::get_mount_type,
    storage::{Storage, ANNOTATION_KEY_STORAGE},
    KUASAR_STATE_DIR,
};

use crate::{
    device::rescan_pci_bus,
    io::{convert_stdio, copy_io_or_console, create_io},
    sandbox::SandboxResources,
    util::{read_io, read_storages, wait_pid},
};
#[cfg(feature = "image-service")]
use std::collections::HashMap;
#[cfg(feature = "image-service")]
use containerd_shim::util::{write_str_to_file, CONFIG_FILE_NAME};
#[cfg(feature = "image-service")]
use crate::image_rpc;


pub const INIT_PID_FILE: &str = "init.pid";

pub type ExecProcess = ProcessTemplate<KuasarExecLifecycle>;
pub type InitProcess = ProcessTemplate<KuasarInitLifecycle>;

pub type KuasarContainer = ContainerTemplate<InitProcess, ExecProcess, KuasarExecFactory>;

#[derive(Clone)]
pub(crate) struct KuasarFactory {
    sandbox: Arc<Mutex<SandboxResources>>,
}

pub struct KuasarExecFactory {
    runtime: Runc,
    bundle: String,
    io_uid: u32,
    io_gid: u32,
}

pub struct KuasarExecLifecycle {
    runtime: Runc,
    bundle: String,
    container_id: String,
    io_uid: u32,
    io_gid: u32,
    spec: Process,
    exit_signal: Arc<ExitSignal>,
}

pub struct KuasarInitLifecycle {
    runtime: Runc,
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
    #[instrument(skip_all)]
    async fn create(
        &self,
        ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<KuasarContainer> {
        rescan_pci_bus().await?;
        let bundle = format!("{}/{}", KUASAR_STATE_DIR, req.id);
        let mut spec: Spec = read_spec(&bundle).await?;
        debug!("req info: {:?} \n spec:{:?}", &req, &spec);
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

        #[cfg(feature = "image-service")]
        {
            self.pull_image_for_container(&annotations, &mut spec, &bundle, req.id()).await?;
        }

        let mut opts = Options::new();
        if let Some(any) = req.options.as_ref() {
            let mut input = CodedInputStream::from_bytes(any.value.as_ref());
            opts.merge_from(&mut input)?;
        }
        if opts.compute_size() > 0 {
            debug!("create options: {:?}", &opts);
        }
        let runtime = opts.binary_name.as_str();

        // As the rootfs is already mounted when handling the storage, the root in spec is one of the
        // storage mount point. so no need to mount rootfs anymore
        let runc = create_runc(
            runtime,
            ns,
            &bundle,
            &opts,
            Some(Arc::new(ShimExecutor::default())),
        )?;

        let id = req.id();

        let stdio = match read_io(&bundle, req.id(), None).await {
            Ok(io) => Stdio::new(&io.stdin, &io.stdout, &io.stderr, io.terminal),
            Err(_) => Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal()),
        };

        // for qemu, the io path is pci address for virtio-serial
        // that needs to be converted to the serial file path
        let stdio = convert_stdio(&stdio).await?;

        let mut init = InitProcess::new(
            id,
            stdio,
            KuasarInitLifecycle::new(runc.clone(), opts.clone(), &bundle),
        );

        self.do_create(&mut init).await?;
        let container = KuasarContainer {
            id: id.to_string(),
            bundle: bundle.to_string(),
            init,
            process_factory: KuasarExecFactory {
                runtime: runc,
                bundle: bundle.to_string(),
                io_uid: opts.io_uid,
                io_gid: opts.io_gid,
            },
            processes: Default::default(),
        };
        Ok(container)
    }

    #[instrument(skip_all)]
    async fn cleanup(&self, _ns: &str, c: &KuasarContainer) -> containerd_shim::Result<()> {
        self.sandbox.lock().await.defer_storages(&c.id).await?;
        #[cfg(feature = "image-service")]
        {
            let image_client = image_rpc::KuasarImageClient::singleton().await?;
            image_client.del_image_config(&c.id).await?;
        }
        Ok(())
    }
}

impl KuasarFactory {
    pub fn new(sandbox: Arc<Mutex<SandboxResources>>) -> Self {
        Self { sandbox }
    }

    #[cfg(feature = "image-service")]
    async fn pull_image_for_container(
        &self,
        annotations: &HashMap<String, String>,
        spec: &mut Spec,
        bundle: &str,
        container_id: &str,
    ) -> Result<()> {
        let image_name = annotations.get(image_rpc::ANNO_K8S_IMAGE_NAME)
            .map_or_else(|| Err(other!("Container id: {:?} can not get image: {:?}", container_id, image_rpc::ANNO_K8S_IMAGE_NAME)),
                        |name| Ok(name)
            )?;
        debug!("image name: {:?}, Container id: {:?}", image_name, container_id);

        let image_client = image_rpc::KuasarImageClient::singleton().await?;
        let root_path = image_client
            .pull_image_for_container(&image_name, &container_id)
            .await?;
        // merge the image bundle OCI spec into container spec after pull and mount image
        image_client.merge_bundle_oci(spec).await?;
        
        let mut root = spec.root().clone().unwrap_or_default();
        root.set_path(std::path::PathBuf::from(root_path));
        spec.set_root(Some(root));
        debug!("new spec:{:?}", &spec);

        let spec_content_new = serde_json::to_string(&spec)
            .map_err(other_error!(e, "failed to marshal spec to string"))?;
        write_str_to_file(format!("{}/{}", &bundle, CONFIG_FILE_NAME), &spec_content_new).await?;
        Ok(())
    }


    #[instrument(skip_all)]
    async fn do_create(&self, init: &mut InitProcess) -> Result<()> {
        let id = init.id.to_string();
        let stdio = &init.stdio;
        let opts = &init.lifecycle.opts;
        let bundle = &init.lifecycle.bundle;
        let pid_path = Path::new(bundle).join(INIT_PID_FILE);
        let mut no_pivot_root = opts.no_pivot_root;
        // pivot_root could not work with initramfs
        match get_mount_type("/") {
            Ok(m_type) => {
                if m_type == *"rootfs" {
                    no_pivot_root = true;
                }
            }
            Err(e) => debug!("get mount type failed {}", e),
        };
        let mut create_opts = runc::options::CreateOpts::new()
            .pid_file(&pid_path)
            .no_pivot(no_pivot_root)
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
            return Err(runtime_error(bundle, e, "OCI runtime create failed").await);
        }
        copy_io_or_console(init, socket, pio, init.lifecycle.exit_signal.clone()).await?;
        let pid = read_file_to_str(pid_path).await?.parse::<i32>()?;
        init.pid = pid;
        Ok(())
    }
}

// runtime_error will read the OCI runtime logfile retrieving OCI runtime error
pub async fn runtime_error(bundle: &str, r_err: runc::error::Error, msg: &str) -> Error {
    match get_last_runtime_error(bundle).await {
        Err(e) => other!(
            "{}: unable to retrieve OCI runtime error ({}): {}",
            msg,
            e,
            r_err
        ),
        Ok(rt_msg) => {
            if rt_msg.is_empty() {
                other!("{}: empty msg in log file: {}", msg, r_err)
            } else {
                other!("{}: {}", msg, rt_msg)
            }
        }
    }
}

async fn get_last_runtime_error(bundle: &str) -> Result<String> {
    let log_path = Path::new(bundle).join("log.json");
    let mut rt_msg = String::new();
    match File::open(log_path).await {
        Err(e) => Err(other!("unable to open OCI runtime log file: {}", e)),
        Ok(file) => {
            let mut reader = BufReader::new(file);
            let file_size = reader
                .seek(SeekFrom::End(0))
                .await
                .map_err(other_error!(e, "error seek from start"))?;

            let mut pre_buffer: Option<Vec<u8>> = None;
            let mut buffer = Vec::new();

            for offset in (0..file_size).rev() {
                if offset == 0 {
                    break;
                }
                reader
                    .seek(SeekFrom::Start(offset))
                    .await
                    .map_err(other_error!(e, "error seek"))?;
                let result = reader
                    .read_until(b'\n', &mut buffer)
                    .await
                    .map_err(other_error!(e, "reading from cursor fail"))?;
                if result == 1 && pre_buffer.is_some() {
                    let line = String::from_utf8_lossy(&pre_buffer.unwrap()).into_owned();
                    match serde_json::from_str::<Log>(&line) {
                        Err(e) => return Err(other!("unable to parse log msg({}): {}", line, e)),
                        Ok(log) => {
                            if log.level == "error" {
                                rt_msg = log.msg.trim().to_string();
                                break;
                            }
                        }
                    }
                }
                pre_buffer = Some(buffer.clone());
                buffer.clear();
            }
            Ok(rt_msg)
        }
    }
}

#[async_trait]
impl ProcessFactory<ExecProcess> for KuasarExecFactory {
    #[instrument(skip_all)]
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
            lifecycle: Arc::from(KuasarExecLifecycle {
                runtime: self.runtime.clone(),
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

#[async_trait]
impl ProcessLifecycle<InitProcess> for KuasarInitLifecycle {
    #[instrument(skip_all)]
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        if let Err(e) = self.runtime.start(p.id.as_str()).await {
            return Err(runtime_error(&p.lifecycle.bundle, e, "OCI runtime start failed").await);
        }
        p.state = Status::RUNNING;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn kill(
        &self,
        p: &mut InitProcess,
        signal: u32,
        all: bool,
    ) -> containerd_shim::Result<()> {
        if let Err(r_err) = self
            .runtime
            .kill(
                p.id.as_str(),
                signal,
                Some(&runc::options::KillOpts { all }),
            )
            .await
        {
            let e = runtime_error(&p.lifecycle.bundle, r_err, "OCI runtime kill failed").await;

            return Err(check_kill_error(e.to_string()));
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        if let Err(e) = self
            .runtime
            .delete(
                p.id.as_str(),
                Some(&runc::options::DeleteOpts { force: true }),
            )
            .await
        {
            if !e.to_string().to_lowercase().contains("does not exist") {
                return Err(
                    runtime_error(&p.lifecycle.bundle, e, "OCI runtime delete failed").await,
                );
            }
        }
        self.exit_signal.signal();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[instrument(skip_all)]
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
    #[instrument(skip_all)]
    async fn update(&self, _p: &mut InitProcess, _resources: &LinuxResources) -> Result<()> {
        Err(Error::Unimplemented("update resource".to_string()))
    }

    #[cfg(target_os = "linux")]
    #[instrument(skip_all)]
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
    #[instrument(skip_all)]
    async fn stats(&self, _p: &InitProcess) -> Result<Metrics> {
        Err(Error::Unimplemented("process stats".to_string()))
    }

    #[instrument(skip_all)]
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
                ..ProcessInfo::default()
            })
            .collect())
    }
}

impl KuasarInitLifecycle {
    #[instrument(skip_all)]
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

#[async_trait]
impl ProcessLifecycle<ExecProcess> for KuasarExecLifecycle {
    #[instrument(skip_all)]
    async fn start(&self, p: &mut ExecProcess) -> containerd_shim::Result<()> {
        rescan_pci_bus().await?;
        let bundle = self.bundle.to_string();
        let pid_path = Path::new(&bundle).join(format!("{}.pid", &p.id));
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
            return Err(runtime_error(&bundle, e, "OCI runtime exec failed").await);
        }
        copy_io_or_console(p, socket, pio, p.lifecycle.exit_signal.clone()).await?;
        let pid = read_file_to_str(pid_path).await?.parse::<i32>()?;
        p.pid = pid;
        p.state = Status::RUNNING;
        Ok(())
    }

    #[instrument(skip_all)]
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

    #[instrument(skip_all)]
    async fn delete(&self, p: &mut ExecProcess) -> Result<()> {
        self.exit_signal.signal();
        let exec_pid_path = Path::new(self.bundle.as_str()).join(format!("{}.pid", p.id));
        remove_file(exec_pid_path).await.unwrap_or_default();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update(&self, _p: &mut ExecProcess, _resources: &LinuxResources) -> Result<()> {
        Err(Error::Unimplemented("exec update".to_string()))
    }

    #[instrument(skip_all)]
    async fn stats(&self, _p: &ExecProcess) -> Result<Metrics> {
        Err(Error::Unimplemented("exec stats".to_string()))
    }

    #[instrument(skip_all)]
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

const DEFAULT_RUNC_ROOT: &str = "/run/containerd/runc";
const DEFAULT_COMMAND: &str = "runc";

#[instrument(skip_all)]
pub fn create_runc(
    runtime: &str,
    namespace: &str,
    bundle: impl AsRef<Path>,
    opts: &Options,
    spawner: Option<Arc<dyn Spawner + Send + Sync>>,
) -> containerd_shim::Result<Runc> {
    let runtime = if runtime.is_empty() {
        DEFAULT_COMMAND
    } else {
        runtime
    };
    let root = opts.root.as_str();
    let root = Path::new(if root.is_empty() {
        DEFAULT_RUNC_ROOT
    } else {
        root
    })
    .join(namespace);

    let log = bundle.as_ref().join("log.json");
    let mut gopts = GlobalOpts::default()
        .command(runtime)
        .root(root)
        .log(log)
        .log_json()
        .systemd_cgroup(opts.systemd_cgroup);
    if let Some(s) = spawner {
        gopts.custom_spawner(s);
    }
    gopts
        .build()
        .map_err(other_error!(e, "unable to create runc instance"))
}

#[derive(Default, Debug)]
pub struct ShimExecutor {}

#[async_trait]
impl Spawner for ShimExecutor {
    #[instrument(skip_all)]
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use containerd_shim::util::{mkdir, write_str_to_file};
    use tokio::fs::remove_dir_all;

    use crate::container::runtime_error;

    #[tokio::test]
    async fn test_runtime_error_with_logfile() {
        let empty_err = runc::error::Error::NotFound;
        let log_json = "\
        {\"level\":\"info\",\"msg\":\"hello word\",\"time\":\"2022-11-25\"}\n\
        {\"level\":\"error\",\"msg\":\"failed error\",\"time\":\"2022-11-26\"}\n\
        {\"level\":\"error\",\"msg\":\"panic\",\"time\":\"2022-11-27\"}\n\
        {\"level\":\"info\",\"msg\":\"program exit\",\"time\":\"2024-1-24\"}\n\
        ";
        let test_dir = "/tmp/kuasar-test_runtime_error_with_logfile";
        let _ = mkdir(test_dir, 0o711).await;
        let test_log_file = Path::new(test_dir).join("log.json");

        assert!(
            write_str_to_file(test_log_file.as_path(), log_json)
                .await
                .is_ok(),
            "write log json should not be error"
        );

        let expected_msg = "panic";
        let actual_err = runtime_error(
            test_dir,
            empty_err,
            "test_runtime_error_with_logfile failed",
        )
        .await;
        assert!(remove_dir_all(test_dir).await.is_ok(), "remove test dir");
        assert!(
            actual_err.to_string().contains(expected_msg),
            "actual error \"{}\" should contains \"{}\"",
            actual_err.to_string(),
            expected_msg
        );
    }

    #[tokio::test]
    async fn test_runtime_error_without_logfile() {
        let empty_err = runc::error::Error::NotFound;
        let test_dir = "/tmp/kuasar-test_runtime_error_without_logfile";
        let _ = remove_dir_all(test_dir).await;
        assert!(
            !Path::new(test_dir).exists(),
            "{} should not exist",
            test_dir
        );

        let expected_msg = "Unable to locate the runc";
        let actual_err = runtime_error(
            test_dir,
            empty_err,
            "test_runtime_error_without_logfile failed",
        )
        .await;
        assert!(
            actual_err.to_string().contains(expected_msg),
            "actual error \"{}\" should contains \"{}\"",
            actual_err.to_string(),
            expected_msg
        );
    }
}
