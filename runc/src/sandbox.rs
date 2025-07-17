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
    collections::HashMap,
    io::Write,
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
    sync::Arc,
};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    cri::api::v1::NamespaceMode,
    data::{ContainerData, SandboxData},
    error::{Error, Result},
    signal::ExitSignal,
    Container, ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use log::warn;
use nix::{
    mount::{mount, umount, MsFlags},
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{create_dir_all, remove_dir_all, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock},
};

use crate::{read_count, write_all};

pub struct RuncSandboxer {
    #[allow(clippy::type_complexity)]
    pub(crate) sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<RuncSandbox>>>>>,
    task_address: String,
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
    req: OwnedFd,
    resp: OwnedFd,
}

impl SandboxParent {
    pub fn new(req: OwnedFd, resp: OwnedFd) -> Self {
        Self { req, resp }
    }
    pub fn fork_sandbox_process(&mut self, id: &str, netns: &str) -> Result<i32> {
        let mut req = [0u8; 512];
        (&mut req[0..64]).write_all(id.as_bytes())?;
        (&mut req[64..]).write_all(netns.as_bytes())?;
        write_all(&self.req, &req)?;
        let mut resp = [0u8; 4];
        let r = read_count(self.resp.as_raw_fd(), 4)?;
        resp[..].copy_from_slice(r.as_slice());
        let pid = i32::from_le_bytes(resp);
        Ok(pid)
    }
}

impl RuncSandboxer {
    pub async fn new(sandbox_parent: SandboxParent, task_address: &str) -> Result<Self> {
        Ok(Self {
            task_address: task_address.to_string(),
            sandboxes: Default::default(),
            sandbox_parent: Arc::new(Mutex::new(sandbox_parent)),
        })
    }

    pub async fn recover(&self, dir: &str) -> Result<()> {
        let mut subs = tokio::fs::read_dir(dir).await.map_err(Error::IO)?;
        let mut pids = Vec::new();
        while let Some(entry) = subs.next_entry().await.unwrap() {
            if let Ok(t) = entry.file_type().await {
                if t.is_dir() {
                    let path = Path::new(dir).join(entry.file_name());
                    match RuncSandbox::recover(&path).await {
                        Ok(sb) => {
                            if let SandboxStatus::Running(pid) = sb.status {
                                // TODO need to check if the sandbox process is still running.
                                pids.push(pid);
                            }
                            let sb_mutex = Arc::new(Mutex::new(sb));
                            self.sandboxes
                                .write()
                                .await
                                .insert(entry.file_name().to_str().unwrap().to_string(), sb_mutex);
                        }
                        Err(e) => {
                            warn!("failed to recover sandbox {:?}, {:?}", entry.file_name(), e);
                            remove_dir_all(&path).await.unwrap_or_default();
                        }
                    }
                }
            }
        }

        Ok(())
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
        sandbox
            .prepare_sandbox_ns(sandbox_pid)
            .await
            .inspect_err(|_| {
                kill(Pid::from_raw(sandbox_pid), Signal::SIGKILL).unwrap_or_default();
            })?;

        sandbox
            .data
            .task_address
            .clone_from(&format!("ttrpc+{}", self.task_address));
        sandbox.dump().await.inspect_err(|_| {
            kill(Pid::from_raw(sandbox_pid), Signal::SIGKILL).unwrap_or_default();
        })?;
        Ok(())
    }

    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let sandbox = self.sandbox(id).await?;
        sandbox.lock().await.data = data;
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
            if pid != 0 {
                kill(Pid::from_raw(pid as i32), Signal::SIGKILL).unwrap_or_default();
            } else {
                let uts_path = format!("{}/uts", self.base_dir);
                umount(&*uts_path)
                    .map_err(|e| anyhow!("failed to umount sandbox uts ns, {}", e))?;
                let ipc_path = format!("{}/ipc", self.base_dir);
                umount(&*ipc_path)
                    .map_err(|e| anyhow!("failed to umount sandbox ipc ns, {}", e))?;
            }
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
        let sb = serde_json::from_slice::<RuncSandbox>(content.as_slice())
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
            .truncate(true)
            .open(&dump_path)
            .await
            .map_err(Error::IO)?;
        dump_file
            .write_all(dump_data.as_slice())
            .await
            .map_err(Error::IO)?;
        Ok(())
    }

    async fn prepare_sandbox_ns(&mut self, sandbox_pid: i32) -> Result<()> {
        if !self.has_shared_pid_namespace() {
            let uts_path = format!("{}/uts", self.base_dir);
            {
                File::create(&uts_path).await.map_err(Error::IO)?;
            }
            mount(
                Some(&*format!("/proc/{}/ns/uts", sandbox_pid)),
                &*uts_path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .map_err(|e| anyhow!("failed to mount sandbox uts ns, {}", e))?;

            let ipc_path = format!("{}/ipc", self.base_dir);
            {
                File::create(&ipc_path).await.map_err(Error::IO)?;
            }
            mount(
                Some(&*format!("/proc/{}/ns/ipc", sandbox_pid)),
                &*ipc_path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .map_err(|e| anyhow!("failed to mount sandbox ipc ns, {}", e))?;

            let net_path = format!("{}/net", self.base_dir);
            {
                File::create(&net_path).await.map_err(Error::IO)?;
            }
            mount(
                Some(&*format!("/proc/{}/ns/net", sandbox_pid)),
                &*net_path,
                None::<&str>,
                MsFlags::MS_BIND,
                None::<&str>,
            )
            .map_err(|e| anyhow!("failed to mount sandbox network ns, {}", e))?;

            kill(Pid::from_raw(sandbox_pid), Signal::SIGKILL).unwrap_or_default();
            self.status = SandboxStatus::Running(0);
        } else {
            self.status = SandboxStatus::Running(sandbox_pid as u32);
        }
        Ok(())
    }

    fn has_shared_pid_namespace(&self) -> bool {
        if let Some(conf) = &self.data.config {
            let a = conf
                .linux
                .as_ref()
                .unwrap()
                .security_context
                .as_ref()
                .unwrap()
                .namespace_options
                .as_ref()
                .unwrap()
                .pid();
            if a == NamespaceMode::Pod {
                return true;
            }
        }
        false
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
        self.containers.insert(
            id.to_string(),
            RuncContainerData {
                data: option.container,
            },
        );
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
