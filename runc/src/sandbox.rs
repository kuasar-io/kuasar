use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{Container, ContainerOption, Sandbox, Sandboxer, SandboxOption, SandboxStatus};
use containerd_sandbox::data::{ContainerData, SandboxData};
use containerd_sandbox::error::{Error, Result};
use containerd_sandbox::signal::ExitSignal;
use containerd_shim::api::Options;
use containerd_shim::asynchronous::monitor::{monitor_subscribe, monitor_unsubscribe};
use containerd_shim::asynchronous::task::TaskService;
use containerd_shim::monitor::{Subject, Topic};
use containerd_shim::processes::Process;
use containerd_shim::protos::shim_async::create_task;
use containerd_shim::protos::ttrpc::asynchronous::Server;
use log::debug;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::NixPath;
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::unistd::{close, fork, ForkResult, pause, Pid};
use os_pipe::{PipeReader, PipeWriter};
use prctl::PrctlMM;
use runc::options::DeleteOpts;
use serde::{Deserialize, Serialize};
use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
use tokio::sync::mpsc::channel;

use crate::{read_count, write_all};
use crate::common::{create_runc, ShimExecutor};
use crate::runc::{recover, RuncContainer, RuncFactory};

pub struct RuncSandboxer {
    #[allow(clippy::type_complexity)]
    pub(crate) sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<RuncSandbox>>>>>,
    task_address: String,
    server: Server,
    sandbox_parent: Arc<Mutex<SandboxParent>>,
}

#[derive(Serialize, Deserialize)]
pub struct RuncSandbox {
    pub(crate) id: String,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) status: SandboxStatus,
    #[serde(skip, default)]
    pub(crate) exit_signal: Arc<ExitSignal>,
    pub(crate) containers: HashMap<String, RuncContainerData>,
}

#[derive(Serialize, Deserialize)]
pub struct RuncContainerData {
    data: ContainerData,
}

impl Container for RuncContainerData {
    fn get_data(&self) -> Result<ContainerData> {
        Ok(self.data.clone())
    }
}

pub struct SandboxParent {
    req: RawFd,
    resp: RawFd,
}

impl SandboxParent {
    pub fn new(req: RawFd, resp: RawFd) -> Self {
        Self {
            req,
            resp,
        }
    }
    pub fn fork_sandbox_process(&mut self, id: &str, netns: &str) -> Result<i32> {
        let mut req = [0u8; 512];
        use std::io::Write;
        unsafe {
            (&mut req[0..64]).write_all(id.as_bytes())?;
            (&mut req[64..]).write_all(netns.as_bytes())?;
        }
        write_all(self.req, &req)?;
        let mut resp = [0u8; 4];
        let mut r = read_count(self.resp, 4)?;
        resp[..].copy_from_slice(r.as_slice());
        let pid = i32::from_le_bytes(resp);
        Ok(pid)
    }
}

impl Drop for SandboxParent {
    fn drop(&mut self) {
        close(self.req).unwrap_or_default();
        close(self.resp).unwrap_or_default();
    }
}

impl RuncSandboxer {
    pub async fn new(sandbox_parent: SandboxParent, task_address: &str) -> Result<Self> {
        let task = start_task_service().await?;
        let task_service = create_task(Arc::new(Box::new(task)));
        let mut server = Server::new().register_service(task_service);
        server = server
            .bind(&task_address)
            .map_err(|e| anyhow!("failed to bind socket {}, {}", task_address, e))?;
        server
            .start()
            .await
            .map_err(|e| anyhow!("failed to start task server, {}", e))?;
        Ok(Self {
            task_address: task_address.to_string(),
            server,
            sandboxes: Default::default(),
            sandbox_parent: Arc::new(Mutex::new(sandbox_parent)),
        })
    }
}

#[async_trait]
impl Sandboxer for RuncSandboxer {
    type Sandbox = RuncSandbox;

    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        let sandbox = RuncSandbox {
            id: id.to_string(),
            base_dir: s.base_dir,
            data: s.sandbox,
            status: SandboxStatus::Created,
            exit_signal: Arc::new(Default::default()),
            containers: Default::default(),
        };
        create_dir_all(&sandbox.base_dir)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", sandbox.base_dir, e))?;
        sandbox.dump().await?;
        let mut sandboxes = self.sandboxes.write().await;
        sandboxes.insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    async fn start(&self, id: &str) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        let mut sandbox = sandbox.lock().await;
        let mut sandbox_parent = self.sandbox_parent.lock().await;
        let sandbox_pid = sandbox_parent.fork_sandbox_process(id, &sandbox.data.netns)?;
        sandbox.status = SandboxStatus::Running(sandbox_pid as u32);
        sandbox.data.task_address = self.task_address.clone();
        sandbox.dump().await?;
        Ok(())
    }

    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        return Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone());
    }

    async fn stop(&self, id: &str, _force: bool) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        sandbox.lock().await.stop().await?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.sandboxes.write().await.remove(id);
        Ok(())
    }
}

impl RuncSandbox {
    async fn stop(&mut self) -> Result<()> {
        if let SandboxStatus::Running(pid) = self.status {
            kill(Pid::from_raw(pid as i32), Signal::SIGKILL)
                .map_err(|e| anyhow!("failed to kill sandbox process {}", e))?;
        }
        let ts = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        self.status = SandboxStatus::Stopped(0, ts);
        self.exit_signal.signal();
        Ok(())
    }

    async fn recover<P: AsRef<Path>>(base_dir: P) -> Result<Self> {
        let dump_path = base_dir.as_ref().join("sandbox.json");
        let mut dump_file = OpenOptions::new()
            .read(true)
            .open(&dump_path)
            .await
            .map_err(Error::IO)?;
        let mut content = vec![];
        dump_file
            .read_to_end(&mut content)
            .await
            .map_err(Error::IO)?;
        let mut sb = serde_json::from_slice::<RuncSandbox>(content.as_slice())
            .map_err(|e| anyhow!("failed to deserialize sandbox, {}", e))?;
        Ok(sb)
    }

    async fn dump(&self) -> Result<()> {
        let dump_data =
            serde_json::to_vec(&self).map_err(|e| anyhow!("failed to serialize sandbox, {}", e))?;
        let dump_path = format!("{}/sandbox.json", self.base_dir);
        let mut dump_file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&dump_path)
            .await
            .map_err(Error::IO)?;
        dump_file
            .write_all(dump_data.as_slice())
            .await
            .map_err(Error::IO)?;
        Ok(())
    }
}

#[async_trait]
impl Sandbox for RuncSandbox {
    type Container = RuncContainerData;

    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn container(&self, id: &str) -> Result<&Self::Container> {
        return self.containers.get(id).ok_or(Error::NotFound(format!(
            "failed to find container by id {id}"
        )));
    }

    async fn append_container(&mut self, id: &str, option: ContainerOption) -> Result<()> {
        self.containers.insert(id.to_string(), RuncContainerData {
            data: option.container
        });
        self.dump().await?;
        Ok(())
    }

    async fn update_container(&mut self, _id: &str, _option: ContainerOption) -> Result<()> {
        Ok(())
    }

    async fn remove_container(&mut self, id: &str) -> Result<()> {
        self.containers.remove(id);
        self.dump().await?;
        Ok(())
    }

    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

// any wasm runtime implementation should implement this function
pub async fn process_exits<F>(task: &TaskService<F, RuncContainer>) {
    let containers = task.containers.clone();
    let exit_signal = task.exit.clone();
    let mut s = monitor_subscribe(Topic::Pid)
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
                        if let Subject::Pid(pid) = e.subject {
                            debug!("receive exit event: {}", &e);
                            let exit_code = e.exit_code;
                            for (_k, cont) in containers.lock().await.iter_mut() {
                                // pid belongs to container init process
                                if cont.init.pid == pid {
                                    // set exit for init process
                                    cont.init.set_exited(exit_code).await;
                                    break;
                                }

                                // pid belongs to container common process
                                for (_exec_id, p) in cont.processes.iter_mut() {
                                    // set exit for exec process
                                    if p.pid == pid {
                                        p.set_exited(exit_code).await;
                                        break;
                                    }
                                }
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

async fn start_task_service() -> Result<TaskService<RuncFactory, RuncContainer>> {
    let (tx, mut rx) = channel(128);
    let factory = RuncFactory::default();
    let containers = recover().await.map_err(|e| anyhow!("failed to recover containers {}", e))?;
    let task = TaskService {
        factory,
        containers: Arc::new(Mutex::new(containers)),
        namespace: "k8s.io".to_string(),
        exit: Arc::new(Default::default()),
        tx: tx.clone(),
    };

    process_exits(&task).await;

    tokio::spawn(async move {
        while let Some((_topic, e)) = rx.recv().await {
            debug!("received event {:?}", e);
        }
    });
    Ok(task)
}