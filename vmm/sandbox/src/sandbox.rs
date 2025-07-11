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

use std::{collections::HashMap, io::ErrorKind, path::Path, sync::Arc};

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    cri::api::v1::NamespaceMode,
    data::SandboxData,
    error::{Error, Result},
    signal::ExitSignal,
    utils::cleanup_mounts,
    ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use containerd_shim::{protos::api::Envelope, util::write_str_to_file};
use log::{debug, error, info, warn};
use protobuf::{well_known_types::any::Any, MessageField};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    fs::{copy, create_dir_all, remove_dir_all, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock},
};
use tracing::instrument;
use ttrpc::context::with_timeout;
use vmm_common::{
    api::{empty::Empty, sandbox::SetupSandboxRequest, sandbox_ttrpc::SandboxServiceClient},
    storage::Storage,
    ETC_HOSTS, ETC_RESOLV, HOSTNAME_FILENAME, HOSTS_FILENAME, RESOLV_FILENAME, SHARED_DIR_SUFFIX,
};

use crate::{
    cgroup::{SandboxCgroup, DEFAULT_CGROUP_PARENT_PATH},
    client::{client_check, client_setup_sandbox, client_sync_clock, new_sandbox_client},
    container::KuasarContainer,
    network::{Network, NetworkConfig},
    utils::{get_dns_config, get_hostname, get_resources, get_sandbox_cgroup_parent_path},
    vm::{Hooks, Recoverable, VMFactory, VM},
};

pub const KUASAR_GUEST_SHARE_DIR: &str = "/run/kuasar/storage/containers/";

pub struct KuasarSandboxer<F: VMFactory, H: Hooks<F::VM>> {
    factory: F,
    hooks: H,
    #[allow(dead_code)]
    config: SandboxConfig,
    #[allow(clippy::type_complexity)]
    sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + Sync + Send,
{
    pub fn new(config: SandboxConfig, vmm_config: F::Config, hooks: H) -> Self {
        Self {
            factory: F::new(vmm_config),
            hooks,
            config,
            sandboxes: Arc::new(Default::default()),
        }
    }
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + DeserializeOwned + Recoverable + Sync + Send + 'static,
{
    #[instrument(skip_all)]
    pub async fn recover(&mut self, dir: &str) {
        let mut subs = match tokio::fs::read_dir(dir).await {
            Ok(subs) => subs,
            Err(e) => {
                error!("FATAL! read working dir {} for recovery: {}", dir, e);
                return;
            }
        };
        while let Some(entry) = subs.next_entry().await.unwrap() {
            if let Ok(t) = entry.file_type().await {
                if !t.is_dir() {
                    continue;
                }
                debug!("recovering sandbox {:?}", entry.file_name());
                let path = Path::new(dir).join(entry.file_name());
                match KuasarSandbox::recover(&path).await {
                    Ok(sb) => {
                        let status = sb.status.clone();
                        let sb_mutex = Arc::new(Mutex::new(sb));
                        // Only running sandbox should be monitored.
                        if let SandboxStatus::Running(_) = status {
                            let sb_clone = sb_mutex.clone();
                            monitor(sb_clone);
                        }
                        self.sandboxes
                            .write()
                            .await
                            .insert(entry.file_name().to_str().unwrap().to_string(), sb_mutex);
                    }
                    Err(e) => {
                        warn!("failed to recover sandbox {:?}, {:?}", entry.file_name(), e);
                        cleanup_mounts(path.to_str().unwrap())
                            .await
                            .unwrap_or_default();
                        remove_dir_all(&path).await.unwrap_or_default();
                    }
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct KuasarSandbox<V: VM> {
    pub(crate) vm: V,
    pub(crate) id: String,
    pub(crate) status: SandboxStatus,
    pub(crate) base_dir: String,
    pub(crate) data: SandboxData,
    pub(crate) containers: HashMap<String, KuasarContainer>,
    pub(crate) storages: Vec<Storage>,
    pub(crate) id_generator: u32,
    pub(crate) network: Option<Network>,
    #[serde(skip, default)]
    pub(crate) client: Arc<Mutex<Option<SandboxServiceClient>>>,
    #[serde(skip, default)]
    pub(crate) exit_signal: Arc<ExitSignal>,
    #[serde(default)]
    pub(crate) sandbox_cgroups: SandboxCgroup,
}

#[async_trait]
impl<F, H> Sandboxer for KuasarSandboxer<F, H>
where
    F: VMFactory + Sync + Send,
    F::VM: VM + Sync + Send + 'static,
    H: Hooks<F::VM> + Sync + Send,
{
    type Sandbox = KuasarSandbox<F::VM>;

    #[instrument(skip_all)]
    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        if self.sandboxes.read().await.get(id).is_some() {
            return Err(Error::AlreadyExist("sandbox".to_string()));
        }

        let mut sandbox_cgroups = SandboxCgroup::default();
        let cgroup_parent_path = get_sandbox_cgroup_parent_path(&s.sandbox)
            .unwrap_or(DEFAULT_CGROUP_PARENT_PATH.to_string());
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // Create sandbox's cgroup and apply sandbox's resources limit
            let create_and_update_sandbox_cgroup = (|| {
                sandbox_cgroups =
                    SandboxCgroup::create_sandbox_cgroups(&cgroup_parent_path, &s.sandbox.id)?;
                sandbox_cgroups.update_res_for_sandbox_cgroups(&s.sandbox)?;
                Ok(())
            })();
            // If create and update sandbox cgroup failed, do rollback operation
            if let Err(e) = create_and_update_sandbox_cgroup {
                let _ = sandbox_cgroups.remove_sandbox_cgroups();
                return Err(e);
            }
        }
        let vm = self.factory.create_vm(id, &s).await?;
        let mut sandbox = KuasarSandbox {
            vm,
            id: id.to_string(),
            status: SandboxStatus::Created,
            base_dir: s.base_dir,
            data: s.sandbox.clone(),
            containers: Default::default(),
            storages: vec![],
            id_generator: 0,
            network: None,
            client: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(ExitSignal::default()),
            sandbox_cgroups,
        };

        // setup sandbox files: hosts, hostname and resolv.conf for guest
        sandbox.setup_sandbox_files().await?;
        self.hooks.post_create(&mut sandbox).await?;
        sandbox.dump().await?;
        self.sandboxes
            .write()
            .await
            .insert(id.to_string(), Arc::new(Mutex::new(sandbox)));
        Ok(())
    }

    #[instrument(skip_all)]
    async fn start(&self, id: &str) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_start(&mut sandbox).await?;

        // Prepare pod network if it has a private network namespace
        if !sandbox.data.netns.is_empty() {
            sandbox.prepare_network().await?;
        }

        if let Err(e) = sandbox.start().await {
            sandbox.destroy_network().await;
            return Err(e);
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);

        if let Err(e) = sandbox.add_to_cgroup().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in add to cgroup {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = self.hooks.post_start(&mut sandbox).await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox post start {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        if let Err(e) = sandbox.dump().await {
            if let Err(re) = sandbox.stop(true).await {
                warn!("roll back in sandbox start dump {}", re);
                return Err(e);
            }
            sandbox.destroy_network().await;
            return Err(e);
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn update(&self, id: &str, data: SandboxData) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        sandbox.data = data;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn sandbox(&self, id: &str) -> Result<Arc<Mutex<Self::Sandbox>>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?
            .clone())
    }

    #[instrument(skip_all)]
    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_stop(&mut sandbox).await?;
        sandbox.stop(force).await?;
        self.hooks.post_stop(&mut sandbox).await?;
        sandbox.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete(&self, id: &str) -> Result<()> {
        let sb_clone = self.sandboxes.read().await.clone();
        if let Some(sb_mutex) = sb_clone.get(id) {
            let mut sb = sb_mutex.lock().await;
            sb.stop(true).await?;

            // Currently only support cgroup V1, cgroup V2 is not supported now
            if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
                // remove the sandbox cgroups
                sb.sandbox_cgroups.remove_sandbox_cgroups()?;
            }

            cleanup_mounts(&sb.base_dir).await?;
            // Should Ignore the NotFound error of base dir as it may be already deleted.
            if let Err(e) = remove_dir_all(&sb.base_dir).await {
                if e.kind() != ErrorKind::NotFound {
                    return Err(e.into());
                }
            }
        }
        self.sandboxes.write().await.remove(id);
        Ok(())
    }
}

#[async_trait]
impl<V> Sandbox for KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    type Container = KuasarContainer;

    #[instrument(skip_all)]
    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    #[instrument(skip_all)]
    async fn ping(&self) -> Result<()> {
        self.vm.ping().await
    }

    #[instrument(skip_all)]
    async fn container<'a>(&'a self, id: &str) -> Result<&'a Self::Container> {
        let container = self
            .containers
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        Ok(container)
    }

    #[instrument(skip_all)]
    async fn append_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_append_handlers(id, options)?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_update_handlers(id, options).await?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn remove_container(&mut self, id: &str) -> Result<()> {
        self.deference_container_storages(id).await?;

        let bundle = format!("{}/{}", self.get_sandbox_shared_path(), &id);
        if let Err(e) = tokio::fs::remove_dir_all(&*bundle).await {
            if e.kind() != ErrorKind::NotFound {
                return Err(anyhow!("failed to remove bundle {}, {}", bundle, e).into());
            }
        }
        let container = self.containers.remove(id);
        // TODO: remove processes first?
        match container {
            None => {}
            Some(c) => {
                for device_id in c.io_devices {
                    self.vm.hot_detach(&device_id).await?;
                }
            }
        }
        self.dump().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    #[instrument(skip_all)]
    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    #[instrument(skip_all)]
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
}

impl<V> KuasarSandbox<V>
where
    V: VM + DeserializeOwned + Recoverable + Sync + Send,
{
    #[instrument(skip_all)]
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
        let mut sb = serde_json::from_slice::<KuasarSandbox<V>>(content.as_slice())
            .map_err(|e| anyhow!("failed to deserialize sandbox, {}", e))?;
        if let SandboxStatus::Running(_) = sb.status {
            if let Err(e) = sb.vm.recover().await {
                warn!("failed to recover vm {}: {}, then force kill it!", sb.id, e);
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            };
            if let Err(e) = sb.init_client().await {
                if let Err(re) = sb.stop(true).await {
                    warn!("roll back in recover, init task client and stop: {}", re);
                    return Err(e);
                }
                return Err(e);
            }
            sb.sync_clock().await;
            sb.forward_events().await;
        }
        // recover the sandbox_cgroups in the sandbox object
        sb.sandbox_cgroups =
            SandboxCgroup::create_sandbox_cgroups(&sb.sandbox_cgroups.cgroup_parent_path, &sb.id)?;

        Ok(sb)
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    #[instrument(skip_all)]
    async fn start(&mut self) -> Result<()> {
        let pid = self.vm.start().await?;

        if let Err(e) = self.init_client().await {
            if let Err(re) = self.vm.stop(true).await {
                warn!("roll back in init task client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        if let Err(e) = self.setup_sandbox().await {
            if let Err(re) = self.vm.stop(true).await {
                error!("roll back in setup sandbox client: {}", re);
                return Err(e);
            }
            return Err(e);
        }

        self.forward_events().await;

        self.status = SandboxStatus::Running(pid);
        Ok(())
    }

    #[instrument(skip_all)]
    async fn stop(&mut self, force: bool) -> Result<()> {
        match self.status {
            // If a sandbox is created:
            // 1. Just Created, vmm is not running: roll back and cleanup
            // 2. Created and vmm is running: roll back and cleanup
            // 3. Created and vmm is exited abnormally after running: status is Stopped
            SandboxStatus::Created => {}
            SandboxStatus::Running(_) => {}
            SandboxStatus::Stopped(_, _) => {
                // Network should already be destroyed when sandbox is stopped.
                self.destroy_network().await;
                return Ok(());
            }
            _ => {
                return Err(
                    anyhow!("sandbox {} is in {:?} while stop", self.id, self.status).into(),
                );
            }
        }
        let container_ids: Vec<String> = self.containers.keys().map(|k| k.to_string()).collect();
        if force {
            for id in container_ids {
                self.remove_container(&id).await.unwrap_or_default();
            }
        } else {
            for id in container_ids {
                self.remove_container(&id).await?;
            }
        }

        self.vm.stop(force).await?;
        self.destroy_network().await;
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) fn container_mut(&mut self, id: &str) -> Result<&mut KuasarContainer> {
        self.containers
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("no container with id {}", id)))
    }

    #[instrument(skip_all)]
    pub(crate) fn increment_and_get_id(&mut self) -> u32 {
        self.id_generator += 1;
        self.id_generator
    }

    #[instrument(skip_all)]
    async fn init_client(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client(&addr).await?;
            debug!("connected to task server {}", self.id);
            client_check(&client).await?;
            *client_guard = Some(client)
        }
        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn setup_sandbox(&mut self) -> Result<()> {
        let mut req = SetupSandboxRequest::new();

        if let Some(client) = &*self.client.lock().await {
            // Set PodSandboxConfig
            if let Some(config) = &self.data.config {
                let config_str = serde_json::to_vec(config).map_err(|e| {
                    Error::Other(anyhow!(
                        "failed to marshal PodSandboxConfig to string, {:?}",
                        e
                    ))
                })?;

                let mut any = Any::new();
                any.type_url = "PodSandboxConfig".to_string();
                any.value = config_str;

                req.config = MessageField::some(any);
            }

            if let Some(network) = self.network.as_ref() {
                // Set interfaces
                req.interfaces = network.interfaces().iter().map(|x| x.into()).collect();

                // Set routes
                req.routes = network.routes().iter().map(|x| x.into()).collect();
            }

            client_setup_sandbox(client, &req).await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn sync_clock(&self) {
        if let Some(client) = &*self.client.lock().await {
            client_sync_clock(client, self.id.as_str(), self.exit_signal.clone());
        }
    }

    #[instrument(skip_all)]
    async fn setup_sandbox_files(&self) -> Result<()> {
        let shared_path = self.get_sandbox_shared_path();
        create_dir_all(&shared_path)
            .await
            .map_err(|e| anyhow!("create host sandbox path({}): {}", shared_path, e))?;

        // Handle hostname
        let mut hostname = get_hostname(&self.data);
        if hostname.is_empty() {
            hostname = hostname::get()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        hostname.push('\n');
        let hostname_path = Path::new(&shared_path).join(HOSTNAME_FILENAME);
        write_str_to_file(hostname_path, &hostname)
            .await
            .map_err(|e| anyhow!("write hostname: {:?}", e))?;

        // handle hosts
        let hosts_path = Path::new(&shared_path).join(HOSTS_FILENAME);
        copy(ETC_HOSTS, hosts_path)
            .await
            .map_err(|e| anyhow!("copy hosts: {}", e))?;

        // handle resolv.conf
        let resolv_path = Path::new(&shared_path).join(RESOLV_FILENAME);
        match get_dns_config(&self.data).map(|dns_config| {
            parse_dnsoptions(
                &dns_config.servers,
                &dns_config.searches,
                &dns_config.options,
            )
        }) {
            Some(resolv_content) if !resolv_content.is_empty() => {
                write_str_to_file(resolv_path, &resolv_content)
                    .await
                    .map_err(|e| anyhow!("write reslov.conf: {:?}", e))?;
            }
            _ => {
                copy(ETC_RESOLV, resolv_path)
                    .await
                    .map_err(|e| anyhow!("copy resolv.conf: {}", e))?;
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn get_sandbox_shared_path(&self) -> String {
        format!("{}/{}", self.base_dir, SHARED_DIR_SUFFIX)
    }

    #[instrument(skip_all)]
    pub async fn prepare_network(&mut self) -> Result<()> {
        // get vcpu for interface queue, at least one vcpu
        let mut vcpu = 1;
        if let Some(resources) = get_resources(&self.data) {
            if resources.cpu_period > 0 && resources.cpu_quota > 0 {
                // get ceil of cpus if it is not integer
                let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
                vcpu = base as u32;
            }
        }

        let network_config = NetworkConfig {
            netns: self.data.netns.to_string(),
            sandbox_id: self.id.to_string(),
            queue: vcpu,
        };
        let network = Network::new(network_config).await?;
        network.attach_to(self).await?;
        Ok(())
    }

    //  If a sandbox is still running, destroy network may hang with its running
    #[instrument(skip_all)]
    pub async fn destroy_network(&mut self) {
        // Network should be destroyed only once, take it out here.
        if let Some(mut network) = self.network.take() {
            network.destroy().await;
        }
    }

    #[instrument(skip_all)]
    pub async fn add_to_cgroup(&self) -> Result<()> {
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // add vmm process into sandbox cgroup
            if let SandboxStatus::Running(vmm_pid) = self.status {
                let vcpu_threads = self.vm.vcpus().await?;
                debug!(
                    "vmm process pid: {}, vcpu threads pid: {:?}",
                    vmm_pid, vcpu_threads
                );
                self.sandbox_cgroups
                    .add_process_into_sandbox_cgroups(vmm_pid, Some(vcpu_threads))?;
                // move all vmm-related process into sandbox cgroup
                for pid in self.vm.pids().affiliated_pids {
                    self.sandbox_cgroups
                        .add_process_into_sandbox_cgroups(pid, None)?;
                }
            } else {
                return Err(Error::Other(anyhow!(
                    "sandbox status is not Running after started!"
                )));
            }
        }
        Ok(())
    }

    pub(crate) async fn forward_events(&mut self) {
        if let Some(client) = &*self.client.lock().await {
            let client = client.clone();
            let exit_signal = self.exit_signal.clone();
            tokio::spawn(async move {
                let fut = async {
                    loop {
                        match client.get_events(with_timeout(0), &Empty::new()).await {
                            Ok(resp) => {
                                if let Err(e) =
                                    crate::client::publish_event(convert_envelope(resp)).await
                                {
                                    error!("{}", e);
                                }
                            }
                            Err(err) => {
                                // if sandbox was closed, will get error Socket("early eof"),
                                // so we should handle errors except this unexpected EOF error.
                                if let ttrpc::error::Error::Socket(s) = &err {
                                    if s.contains("early eof") {
                                        break;
                                    }
                                }
                                error!("failed to get oom event error {:?}", err);
                                break;
                            }
                        }
                    }
                };

                tokio::select! {
                    _ = fut => {},
                    _ = exit_signal.wait() => {},
                }
            });
        }
    }
}

// parse_dnsoptions parse DNS options into resolv.conf format content,
// if none option is specified, will return empty with no error.
fn parse_dnsoptions(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut resolv_content = String::new();

    if !searches.is_empty() {
        resolv_content.push_str(&format!("search {}\n", searches.join(" ")));
    }

    if !servers.is_empty() {
        resolv_content.push_str(&format!("nameserver {}\n", servers.join("\nnameserver ")));
    }

    if !options.is_empty() {
        resolv_content.push_str(&format!("options {}\n", options.join(" ")));
    }

    resolv_content
}

pub fn has_shared_pid_namespace(data: &SandboxData) -> bool {
    if let Some(conf) = &data.config {
        if let Some(pid_ns_mode) = conf
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .and_then(|s| s.namespace_options.as_ref())
            .map(|n| n.pid())
        {
            return pid_ns_mode == NamespaceMode::Pod;
        }
    }
    false
}

fn convert_envelope(envelope: vmm_common::api::events::Envelope) -> Envelope {
    Envelope {
        timestamp: envelope.timestamp,
        namespace: envelope.namespace,
        topic: envelope.topic,
        event: envelope.event,
        special_fields: protobuf::SpecialFields::default(),
    }
}

#[derive(Default, Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub enable_tracing: bool,
}

impl SandboxConfig {
    pub fn log_level(&self) -> String {
        self.log_level.to_string()
    }

    pub fn enable_tracing(&self) -> bool {
        self.enable_tracing
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDeviceSpec {
    #[serde(default)]
    pub(crate) _host_path: Vec<String>,
    #[serde(default)]
    pub(crate) _bdf: Vec<String>,
}

fn monitor<V: VM + 'static>(sandbox_mutex: Arc<Mutex<KuasarSandbox<V>>>) {
    tokio::spawn(async move {
        let mut rx = {
            let sandbox = sandbox_mutex.lock().await;
            if let SandboxStatus::Running(_) = sandbox.status.clone() {
                if let Some(rx) = sandbox.vm.wait_channel().await {
                    rx
                } else {
                    error!("can not get wait channel when sandbox is running");
                    return;
                }
            } else {
                info!(
                    "sandbox {} is {:?} when monitor",
                    sandbox.id, sandbox.status
                );
                return;
            }
        };

        let (code, ts) = *rx.borrow();
        if ts == 0 {
            rx.changed().await.unwrap_or_default();
            let (code, ts) = *rx.borrow();
            let mut sandbox = sandbox_mutex.lock().await;
            info!("monitor sandbox {} terminated", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        } else {
            let mut sandbox = sandbox_mutex.lock().await;
            info!("sandbox {} already terminated before monit it", sandbox.id);
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
            // Network destruction should be done after sandbox status changed from running.
            sandbox.destroy_network().await;
            sandbox
                .dump()
                .await
                .map_err(|e| error!("dump sandbox {} in monitor: {}", sandbox.id, e))
                .unwrap_or_default();
        }
    });
}

#[cfg(test)]
mod tests {
    mod dns {
        use crate::sandbox::parse_dnsoptions;

        #[derive(Default)]
        struct DnsConfig {
            servers: Vec<String>,
            searches: Vec<String>,
            options: Vec<String>,
        }

        #[test]
        fn test_parse_empty_dns_option() {
            let dns_test = DnsConfig::default();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(resolv_content.is_empty())
        }

        #[test]
        fn test_parse_non_empty_dns_option() {
            let dns_test = DnsConfig {
                servers: vec!["8.8.8.8", "server.google.com"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                searches: vec![
                    "server0.google.com",
                    "server1.google.com",
                    "server2.google.com",
                    "server3.google.com",
                    "server4.google.com",
                    "server5.google.com",
                    "server6.google.com",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                options: vec!["timeout:1"].into_iter().map(String::from).collect(),
            };
            let expected_content = "search server0.google.com server1.google.com server2.google.com server3.google.com server4.google.com server5.google.com server6.google.com
nameserver 8.8.8.8
nameserver server.google.com
options timeout:1
".to_string();
            let resolv_content =
                parse_dnsoptions(&dns_test.servers, &dns_test.searches, &dns_test.options);
            assert!(!resolv_content.is_empty());
            assert_eq!(resolv_content, expected_content)
        }
    }
}
