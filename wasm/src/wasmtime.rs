/*
Copyright 2023 The Kuasar Authors.

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

use std::{path::Path, sync::Arc};

use containerd_shim::{
    api::{CreateTaskRequest, ExecProcessRequest},
    container::{ContainerFactory, ContainerTemplate, ProcessFactory},
    error::Error,
    io::Stdio,
    io_error,
    monitor::{monitor_notify_by_exec, monitor_subscribe, monitor_unsubscribe, Subject, Topic},
    other, other_error,
    processes::{Process, ProcessLifecycle, ProcessTemplate},
    protos::{cgroups::metrics::Metrics, types::task::ProcessInfo},
    task::TaskService,
    util::{mkdir, mount_rootfs, read_spec},
    ExitSignal,
};
use log::{debug, trace, warn};
use oci_spec::runtime::{LinuxResources, Spec};
use tokio::fs::read;
use wasmtime::{Config, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{tokio::WasiCtxBuilder, WasiCtx};

use crate::utils::{get_args, get_kv_envs, get_memory_limit, get_rootfs};

pub type ExecProcess = ProcessTemplate<WasmtimeExecLifecycle>;
pub type InitProcess = ProcessTemplate<WasmtimeInitLifecycle>;

pub type WasmtimeContainer = ContainerTemplate<InitProcess, ExecProcess, ExecFactory>;

pub struct ExecFactory {}

pub struct WasmtimeExecLifecycle {}

pub struct WasmtimeInitLifecycle {
    _bundle: String,
    spec: Spec,
    _netns: String,
    _exit_signal: Arc<ExitSignal>,
    engine: Engine,
}

#[derive(Default)]
pub struct WasmtimeContainerFactory {
    pub(crate) netns: String,
}

struct WasmtimeContainerData {
    ctx: WasiCtx,
    limiter: StoreLimits,
}

#[async_trait::async_trait]
impl ContainerFactory<WasmtimeContainer> for WasmtimeContainerFactory {
    async fn create(
        &self,
        _ns: &str,
        req: &CreateTaskRequest,
    ) -> containerd_shim::Result<WasmtimeContainer> {
        let mut spec: Spec = read_spec(req.bundle()).await?;
        spec.canonicalize_rootfs(req.bundle())
            .map_err(|e| Error::InvalidArgument(format!("could not canonicalize rootfs: {e}")))?;
        let rootfs = get_rootfs(&spec).ok_or_else(|| {
            Error::InvalidArgument("rootfs is not set in runtime spec".to_string())
        })?;
        mkdir(&rootfs, 0o711).await?;
        for m in req.rootfs() {
            mount_rootfs(m, &rootfs).await?
        }
        let stdio = Stdio::new(req.stdin(), req.stdout(), req.stderr(), req.terminal);
        let exit_signal = Arc::new(Default::default());
        let netns = self.netns.clone();
        let lifecycle =
            WasmtimeInitLifecycle::new(&spec, exit_signal, &netns, req.bundle()).await?;
        let init_process = InitProcess::new(req.id(), stdio, lifecycle);
        Ok(WasmtimeContainer {
            id: req.id.to_string(),
            bundle: req.id.to_string(),
            init: init_process,
            process_factory: ExecFactory {},
            processes: Default::default(),
        })
    }

    async fn cleanup(&self, _ns: &str, _c: &WasmtimeContainer) -> containerd_shim::Result<()> {
        Ok(())
    }
}

impl WasmtimeInitLifecycle {
    pub async fn new(
        spec: &Spec,
        exit_signal: Arc<ExitSignal>,
        netns: &str,
        bundle: &str,
    ) -> containerd_shim::Result<Self> {
        let mut config = Config::new();
        config.async_support(true).epoch_interruption(true);
        let engine = Engine::new(&config).map_err(other_error!(e, "failed to new engine"))?;

        let res = Self {
            _bundle: bundle.to_string(),
            spec: spec.clone(),
            _netns: netns.to_string(),
            _exit_signal: exit_signal,
            engine,
        };
        Ok(res)
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle<InitProcess> for WasmtimeInitLifecycle {
    async fn start(&self, p: &mut InitProcess) -> containerd_shim::Result<()> {
        let args = get_args(&self.spec);
        let envs = get_kv_envs(&self.spec);
        let root = self
            .spec
            .root()
            .as_ref()
            .ok_or(Error::InvalidArgument(
                "rootfs is not set in runtime spec".to_string(),
            ))?
            .path();

        let mut builder = WasiCtxBuilder::new()
            .args(&args)
            .map_err(other_error!(e, "failed to set wasi args"))?
            .envs(envs.as_slice())
            .map_err(other_error!(e, "failed to set wasi envs"))?;
        if !p.stdio.stdout.is_empty() {
            builder = builder.stdout(Box::new(
                open_wasi_cap_std_file(&p.stdio.stdout, false, true).await?,
            ));
        }
        if !p.stdio.stderr.is_empty() {
            builder = builder.stderr(Box::new(
                open_wasi_cap_std_file(&p.stdio.stderr, false, true).await?,
            ));
        }
        if !p.stdio.stdin.is_empty() {
            builder = builder.stdin(Box::new(
                open_wasi_cap_std_file(&p.stdio.stdin, true, false).await?,
            ));
        }
        trace!("set rootfs {} to guest", root.display());
        if root.exists() && root.is_dir() {
            // TODO support file mount, and filesystem mount
            let preopen_dir = cap_std::fs::Dir::from_std_file(
                tokio::fs::File::open(root)
                    .await
                    .map_err(io_error!(e, "failed to open rootfs {}", root.display()))?
                    .into_std()
                    .await,
            );
            builder = builder
                .preopened_dir(preopen_dir, "/")
                .map_err(other_error!(e, "failed to set preopened_dir"))?;
        } else {
            warn!("rootfs should be a directory");
        }
        let ctx = builder.build();

        let mut limits_builder = StoreLimitsBuilder::new();
        if let Some(memory_size) = get_memory_limit(&self.spec).map(|x| x as usize) {
            limits_builder = limits_builder.memory_size(memory_size);
        }
        let limiter = limits_builder.build();

        let mut store = Store::new(&self.engine, WasmtimeContainerData { ctx, limiter });
        store.limiter(|x| &mut x.limiter);
        store.set_epoch_deadline(1);
        debug!("start wasmtime container {}", p.id);

        let mut cmd = args[0].clone();
        if let Some(stripped_cmd) = args[0].strip_prefix(std::path::MAIN_SEPARATOR) {
            cmd = stripped_cmd.to_string();
        }
        let module_path = root.join(&cmd);
        trace!("start running module in {}", module_path.display());
        let module_data = read(&module_path).await.map_err(io_error!(
            e,
            "failed to read module file {}",
            module_path.display()
        ))?;

        // Compilation of modules happens here, it will use rayon to parallelize the compilation tasks,
        // rayon init as many threads as the cpu count, and each thread has a stack size of 4MB.
        // if we run this on a machine with 100 cpus, 400MB memories will be consumed for these stacks
        // if it is necessary to limit this memory, set env of RAYON_NUM_THREADS to a smaller number.
        let module = Module::new(&self.engine, module_data).map_err(other_error!(e, ""))?;
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::tokio::add_to_linker(&mut linker, |cx: &mut WasmtimeContainerData| {
            &mut cx.ctx
        })
        .map_err(other_error!(e, ""))?;
        let instance = linker
            .instantiate_async(&mut store, &module)
            .await
            .map_err(other_error!(e, ""))?;
        let export = instance
            .get_export(&mut store, "_start")
            .ok_or_else(|| other!("_start import doesn't exist in wasm module"))?;
        let func = match export {
            Extern::Func(f) => f,
            _ => {
                return Err(other!("_start is not a function"));
            }
        };

        let id_clone = p.id.clone();
        tokio::spawn(async move {
            match func.call_async(&mut store, &[], &mut []).await {
                Ok(_) => {
                    debug!("function of container {} finished successfully", id_clone);
                    monitor_notify_by_exec(&id_clone, "", 0)
                        .await
                        .unwrap_or_default();
                }
                Err(e) => {
                    debug!(
                        "function of container {} finished with error {}",
                        id_clone, e
                    );
                    // TODO add exit code for wasmtime
                    monitor_notify_by_exec(&id_clone, "", -1)
                        .await
                        .unwrap_or_default();
                }
            }
        });
        Ok(())
    }

    async fn kill(
        &self,
        _p: &mut InitProcess,
        _signal: u32,
        _all: bool,
    ) -> containerd_shim::Result<()> {
        self.engine.increment_epoch();
        Ok(())
    }

    async fn delete(&self, _p: &mut InitProcess) -> containerd_shim::Result<()> {
        self.engine.increment_epoch();
        Ok(())
    }

    async fn update(
        &self,
        _p: &mut InitProcess,
        _resources: &LinuxResources,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "not supported for wasi containers".to_string(),
        ))
    }

    async fn stats(&self, _p: &InitProcess) -> containerd_shim::Result<Metrics> {
        Err(Error::Unimplemented(
            "not supported for wasi containers".to_string(),
        ))
    }

    async fn ps(&self, _p: &InitProcess) -> containerd_shim::Result<Vec<ProcessInfo>> {
        Ok(vec![])
    }
}

#[async_trait::async_trait]
impl ProcessLifecycle<ExecProcess> for WasmtimeExecLifecycle {
    async fn start(&self, _p: &mut ExecProcess) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }

    async fn kill(
        &self,
        _p: &mut ExecProcess,
        _signal: u32,
        _all: bool,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }

    async fn delete(&self, _p: &mut ExecProcess) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }

    async fn update(
        &self,
        _p: &mut ExecProcess,
        _resources: &LinuxResources,
    ) -> containerd_shim::Result<()> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }

    async fn stats(&self, _p: &ExecProcess) -> containerd_shim::Result<Metrics> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }

    async fn ps(&self, _p: &ExecProcess) -> containerd_shim::Result<Vec<ProcessInfo>> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl ProcessFactory<ExecProcess> for ExecFactory {
    async fn create(&self, _req: &ExecProcessRequest) -> containerd_shim::Result<ExecProcess> {
        Err(Error::Unimplemented(
            "exec not supported for wasi containers".to_string(),
        ))
    }
}

async fn open_wasi_cap_std_file<T: AsRef<Path>>(
    path: T,
    read: bool,
    write: bool,
) -> containerd_shim::Result<wasi_cap_std_sync::file::File> {
    debug!(
        "start opening file {} as a wasi cap std file",
        path.as_ref().display()
    );
    let file = tokio::fs::OpenOptions::new()
        .write(write)
        .read(read)
        .open(path.as_ref())
        .await
        .map_err(io_error!(
            e,
            "failed to open file {}",
            path.as_ref().display()
        ))?;
    debug!(
        "convert std file {} to wasi cap std file",
        path.as_ref().display()
    );
    Ok(wasi_cap_std_sync::file::File::from_cap_std(
        cap_std::fs::File::from_std(file.into_std().await),
    ))
}

pub(crate) async fn exec_exits<F>(task: &TaskService<F, WasmtimeContainer>) {
    let containers = task.containers.clone();
    let exit_signal = task.exit.clone();
    let mut s = monitor_subscribe(Topic::Exec)
        .await
        .expect("monitor subscribe failed");
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = exit_signal.wait() => {
                    debug!("sandbox exit, should break");
                    monitor_unsubscribe(s.id).await.unwrap_or_default();
                    return;
                },
                res = s.rx.recv() => {
                    if let Some(e) = res {
                        if let Subject::Exec(cid, _) = &e.subject {
                            debug!("receive exit event: {}", &e);
                            let exit_code = e.exit_code;
                            if let Some(cont) = containers.lock().await.get_mut(cid) {
                                cont.init.set_exited(exit_code).await;
                            }
                        }
                    } else {
                        monitor_unsubscribe(s.id).await.unwrap_or_default();
                        return;
                    }
                }
            }
        }
    });
}
