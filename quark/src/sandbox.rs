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

use std::{collections::HashMap, fs::read_to_string, path::Path, sync::Arc};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    data::{ContainerData, SandboxData},
    error::{Error, Result},
    signal::ExitSignal,
    spec::{
        JsonSpec, Linux, LinuxCPU, LinuxHugepageLimit, LinuxMemory, LinuxNamespace, LinuxResources,
        Process, Root,
    },
    Container, ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use nix::{
    errno::Errno,
    mount::umount,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use prost_types::Any;
use tokio::{
    fs::{create_dir_all, remove_dir_all},
    sync::{Mutex, RwLock},
};

use crate::{
    mount::bind_mount,
    utils::{cleanup_mounts, mount_rootfs, write_file_atomic},
};

pub struct QuarkSandboxer {
    #[allow(clippy::type_complexity)]
    pub(crate) sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<QuarkSandbox>>>>>,
}

pub struct QuarkSandbox {
    pub(crate) _id: String,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) status: SandboxStatus,
    pub(crate) containers: HashMap<String, QuarkContainer>,
    pub(crate) exit_signal: Arc<ExitSignal>,
}

pub struct QuarkContainer {
    pub(crate) data: ContainerData,
}

#[async_trait]
impl Sandboxer for QuarkSandboxer {
    type Sandbox = QuarkSandbox;

    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        let mut sandbox = QuarkSandbox {
            _id: id.to_string(),
            base_dir: s.base_dir,
            data: s.sandbox,
            status: SandboxStatus::Created,
            containers: Default::default(),
            exit_signal: Arc::new(Default::default()),
        };
        create_dir_all(&sandbox.base_dir)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", sandbox.base_dir, e))?;
        let quark_sandbox_bundle = sandbox.get_sandbox_bundle();
        create_dir_all(&quark_sandbox_bundle)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", quark_sandbox_bundle, e))?;
        // create spec from PodSandboxConfig
        if sandbox.data.spec.is_none() {
            sandbox.data.spec = Some(self.create_spec(&sandbox.data)?);
        }
        if let Some(spec) = &sandbox.data.spec {
            // create root path
            if let Some(root) = &spec.root {
                let root_path = Path::new(&root.path);
                let absolute_root = if !root_path.is_absolute() {
                    let abs_path = format!("{}/{}", quark_sandbox_bundle, &root.path);
                    create_dir_all(&abs_path)
                        .await
                        .map_err(|e| anyhow!("failed to create {}, {}", abs_path, e))?;
                    abs_path
                } else {
                    root.path.clone()
                };
                let dev_path = format!("{}/dev", absolute_root);
                create_dir_all(&dev_path)
                    .await
                    .map_err(|e| anyhow!("failed to create {}, {}", dev_path, e))?;
            }
            // write spec to config.json
            let spec_file = format!("{}/config.json", quark_sandbox_bundle);
            let spec_buf = serde_json::to_vec(spec)
                .map_err(|e| anyhow!("failed to decode sandbox spec {}", e))?;
            write_file_atomic(&spec_file, spec_buf.as_slice()).await?;
        }
        let mut sandboxes = self.sandboxes.write().await;
        sandboxes.insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    async fn start(&self, id: &str) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        let mut cmd = tokio::process::Command::new("quark");
        let quark_sandbox_bundle = sandbox.get_sandbox_bundle();
        cmd.arg("-r");
        cmd.arg(&quark_sandbox_bundle);
        cmd.arg("sandbox");
        cmd.current_dir(&quark_sandbox_bundle);
        let task_address = format!("{}/quark-task.sock", sandbox.base_dir);
        cmd.arg("--task-socket");
        cmd.arg(&task_address);
        cmd.arg("--id");
        cmd.arg(id);
        let pid_path = format!("{}/pid", sandbox.base_dir);
        cmd.arg("--pid-file");
        cmd.arg(&pid_path);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to spawn quark sandbox command, {}", e))?;
        child.wait().await?;
        let pid_str = read_to_string(&pid_path)
            .map_err(|e| anyhow!("failed to read file {}, {}", pid_path, e))?;
        let pid = pid_str
            .parse::<u32>()
            .map_err(|e| anyhow!("failed to parse pid {}, {}", pid_str, e))?;
        sandbox.status = SandboxStatus::Running(pid);
        sandbox.data.task_address = format!("unix://{}", task_address);
        Ok(())
    }

    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone())
    }

    async fn stop(&self, id: &str, _force: bool) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        if let SandboxStatus::Running(pid) = sandbox.status {
            match kill(Pid::from_raw(pid as i32), Signal::SIGKILL) {
                Ok(_) => {}
                Err(e) => {
                    if e != Errno::ESRCH {
                        return Err(anyhow!("failed to kill sandbox process {}, {}", pid, e).into());
                    }
                }
            }
        }
        let ts = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        sandbox.status = SandboxStatus::Stopped(0, ts);
        sandbox.exit_signal.signal();
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let sandbox = sandbox_mutex.lock().await;
        cleanup_mounts(&sandbox.base_dir)
            .await
            .map_err(|e| anyhow!("failed to cleanup mounts in {}, {:?}", sandbox.base_dir, e))?;
        remove_dir_all(&sandbox.base_dir).await.map_err(|e| {
            anyhow!(
                "failed to delete sandbox base directory {}, {}",
                sandbox.base_dir,
                e
            )
        })?;
        Ok(())
    }
}

#[async_trait]
impl Sandbox for QuarkSandbox {
    type Container = QuarkContainer;

    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    async fn ping(&self) -> Result<()> {
        if let SandboxStatus::Running(pid) = self.status {
            kill(Pid::from_raw(pid as i32), None)
                .map_err(|_e| anyhow!("failed to send signal 0 to sandbox process {}", pid))?;
        }
        return Ok(());
    }

    async fn container<'a>(&'a self, id: &str) -> Result<&'a Self::Container> {
        self.containers
            .get(id)
            .ok_or(Error::NotFound(format!("no container id {} found", id)))
    }

    async fn append_container(&mut self, id: &str, option: ContainerOption) -> Result<()> {
        let bundle = self.get_container_bundle(id);
        let mut data = option.container;
        create_dir_all(&bundle)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", bundle, e))?;
        let rootfs = format!("{}/rootfs", bundle);
        create_dir_all(&rootfs)
            .await
            .map_err(|e| anyhow!("failed to create {}, {}", bundle, e))?;

        // mount rootfs outside and remove the rootfs mount in spec
        for m in &data.rootfs {
            mount_rootfs(m, &rootfs).await?;
        }
        data.rootfs = vec![];

        let mut unhandled_mount = vec![];
        let spec = data.spec.as_mut().ok_or(anyhow!("no spec in request"))?;
        for m in &spec.mounts {
            // TODO are there any security issues?
            if m.r#type == "bind" {
                let target = format!("{}/{}", rootfs, &m.destination);
                bind_mount(&m.source, &target, m.options.as_slice()).await?;
            } else {
                unhandled_mount.push(m.clone());
            }
        }
        spec.mounts = unhandled_mount;

        if let Some(io) = &mut data.io {
            io.stdin = self.bind_mount_io(id, None, &io.stdin, "stdin").await?;
            io.stdout = self.bind_mount_io(id, None, &io.stdout, "stdout").await?;
            io.stderr = self.bind_mount_io(id, None, &io.stderr, "stderr").await?;
        }
        let config_path = format!("{}/config.json", self.get_container_bundle(id));
        let spec_content = serde_json::to_vec(&spec)
            .map_err(|e| anyhow!("failed to marshal spec {:?}: {}", spec, e))?;
        write_file_atomic(&config_path, spec_content.as_slice()).await?;
        let container = QuarkContainer { data };
        self.containers.insert(id.to_string(), container);
        Ok(())
    }

    async fn update_container(&mut self, id: &str, option: ContainerOption) -> Result<()> {
        let removed_process_ids = {
            let container = self.container(id).await?;

            let mut remove_ids = vec![];
            for p in &container.data.processes {
                let mut removed = true;
                for new_p in &option.container.processes {
                    if p.id == new_p.id {
                        removed = false;
                    }
                }
                if removed {
                    if let Some(io) = &p.io {
                        self.bind_unmount_io(&io.stdin).await?;
                        self.bind_unmount_io(&io.stdout).await?;
                        self.bind_unmount_io(&io.stderr).await?;
                    }
                }
                remove_ids.push(p.id.clone());
            }
            remove_ids
        };
        self.container_mut(id)?
            .data
            .processes
            .retain(|p| !removed_process_ids.contains(&p.id));

        // handle new process
        let new_processes = {
            let mut new_ps = vec![];
            let container = self.container(id).await?;
            let mut option = option;
            while let Some(mut p) = option.container.processes.pop() {
                let mut existed = false;
                for old_p in &container.data.processes {
                    if old_p.id == p.id {
                        existed = true;
                    }
                }
                if !existed {
                    if let Some(io) = &mut p.io {
                        io.stdin = self
                            .bind_mount_io(id, Some(&p.id), &io.stdin, "stdin")
                            .await?;
                        io.stdout = self
                            .bind_mount_io(id, Some(&p.id), &io.stdout, "stdout")
                            .await?;
                        io.stderr = self
                            .bind_mount_io(id, Some(&p.id), &io.stderr, "stderr")
                            .await?;
                    }
                    new_ps.push(p)
                }
            }
            new_ps
        };

        for p in new_processes {
            self.container_mut(id)?.data.processes.push(p);
        }
        Ok(())
    }

    async fn remove_container(&mut self, id: &str) -> Result<()> {
        let bundle = self.get_container_bundle(id);
        cleanup_mounts(&bundle).await?;
        remove_dir_all(&bundle)
            .await
            .map_err(|e| anyhow!("failed to remove bundle {}, {}", bundle, e))?;
        self.containers.remove(id);
        Ok(())
    }

    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl QuarkSandbox {
    fn get_sandbox_bundle(&self) -> String {
        format!("{}/sandbox", self.base_dir)
    }

    fn get_container_bundle(&self, id: &str) -> String {
        format!("{}/sandbox/{}", self.base_dir, id)
    }

    async fn bind_mount_io(
        &self,
        container_id: &str,
        process_id: Option<&str>,
        io_file: &str,
        stdio_type: &str,
    ) -> Result<String> {
        if io_file.is_empty() {
            return Ok("".to_string());
        }
        let std_new = if let Some(p_id) = process_id {
            format!(
                "{}/{}-{}",
                self.get_container_bundle(container_id),
                p_id,
                stdio_type
            )
        } else {
            format!("{}/{}", self.get_container_bundle(container_id), stdio_type)
        };
        bind_mount(io_file, &std_new, &[]).await?;
        let io_file_in = if let Some(p_id) = process_id {
            format!("/{}/{}-{}", container_id, p_id, stdio_type)
        } else {
            format!("/{}/{}", container_id, stdio_type)
        };
        Ok(io_file_in)
    }

    async fn bind_unmount_io(&self, file_path: &str) -> Result<()> {
        if file_path.is_empty() {
            return Ok(());
        }

        let std_path = Path::new(&file_path);
        if std_path.exists() {
            match umount(std_path) {
                Ok(_) => {}
                Err(e) => {
                    if e != Errno::ENOENT {
                        return Err(
                            anyhow!("failed to unmount container io {}, {}", file_path, e).into(),
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn container_mut(&mut self, id: &str) -> Result<&mut <Self as Sandbox>::Container> {
        self.containers
            .get_mut(id)
            .ok_or(Error::NotFound(format!("no container id {} found", id)))
    }
}

impl Container for QuarkContainer {
    fn get_data(&self) -> Result<ContainerData> {
        let data = self.data.clone();
        Ok(data)
    }
}

impl QuarkSandboxer {
    fn create_spec(&self, data: &SandboxData) -> Result<JsonSpec> {
        let mut spec = JsonSpec::default();
        let config = data
            .config
            .as_ref()
            .ok_or(anyhow!("no PodSandboxConfig in request"))?;

        // construct Linux Configs from PodSandboxConfig
        let mut linux = Linux {
            uid_mappings: vec![],
            gid_mappings: vec![],
            sysctl: Default::default(),
            resources: None,
            cgroups_path: "".to_string(),
            namespaces: vec![],
            devices: vec![],
            seccomp: None,
            rootfs_propagation: "".to_string(),
            masked_path: vec![],
            readonly_path: vec![],
            mount_label: "".to_string(),
            intel_rdt: None,
        };
        if let Some(lc) = &config.linux {
            if let Some(resources) = &lc.resources {
                let mut spec_resources = LinuxResources {
                    devices: vec![],
                    memory: None,
                    cpu: None,
                    pids: None,
                    block_io: None,
                    hugepage_limits: vec![],
                    network: None,
                    rdma: Default::default(),
                    files: None,
                };

                let mut memory = LinuxMemory {
                    limit: Some(resources.memory_limit_in_bytes as u64),
                    reservation: None,
                    swap: None,
                    kernel: None,
                    kernel_tcp: None,
                    swappiness: None,
                    disable_oom_killer: None,
                };
                if resources.memory_limit_in_bytes > 0 {
                    memory.limit = Some(resources.memory_limit_in_bytes as u64);
                }
                if resources.memory_swap_limit_in_bytes > 0 {
                    memory.swap = Some(resources.memory_swap_limit_in_bytes as u64);
                }
                let mut cpu = LinuxCPU {
                    shares: None,
                    quota: None,
                    period: None,
                    realtime_runtime: None,
                    realtime_period: None,
                    cpus: resources.cpuset_cpus.to_string(),
                    mems: resources.cpuset_mems.to_string(),
                };
                if resources.cpu_period > 0 {
                    cpu.period = Some(resources.cpu_period as u64);
                }
                if resources.cpu_quota > 0 {
                    cpu.quota = Some(resources.cpu_quota);
                }
                if resources.cpu_shares > 0 {
                    cpu.shares = Some(resources.cpu_shares as u64);
                }
                spec_resources.cpu = Some(cpu);

                if !resources.hugepage_limits.is_empty() {
                    for l in &resources.hugepage_limits {
                        spec_resources.hugepage_limits.push(LinuxHugepageLimit {
                            page_size: l.page_size.to_string(),
                            limit: l.limit,
                        });
                    }
                }
                linux.resources = Some(spec_resources);
            }
        }

        if !data.netns.is_empty() {
            linux.namespaces.push(LinuxNamespace {
                r#type: "network".to_string(),
                path: data.netns.to_string(),
            });
        }
        spec.linux = Some(linux);

        // TODO construct Process from PodSandboxConfig
        let process = Process::new();
        spec.process = Some(process);
        // TODO add root path
        let root = Root {
            path: "rootfs".to_string(),
            readonly: false,
        };
        spec.root = Some(root);
        Ok(spec)
    }
}

pub(crate) fn _to_any(spec: &JsonSpec) -> Result<Any> {
    let spec_vec =
        serde_json::to_vec(spec).map_err(|e| anyhow!("failed to parse sepc to json, {}", e))?;
    Ok(Any {
        type_url: "types.containerd.io/opencontainers/runtime-spec/1/Spec".to_string(),
        value: spec_vec,
    })
}
