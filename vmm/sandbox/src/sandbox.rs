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
    data::SandboxData,
    error::{Error, Result},
    signal::ExitSignal,
    utils::cleanup_mounts,
    ContainerOption, Sandbox, SandboxOption, SandboxStatus, Sandboxer,
};
use containerd_shim::util::write_str_to_file;
use log::{debug, error, info, warn};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    fs::{copy, create_dir_all, remove_dir_all, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, RwLock},
};
use vmm_common::{
    api::sandbox_ttrpc::SandboxServiceClient, storage::Storage, ETC_HOSTS, ETC_RESOLV,
    HOSTNAME_FILENAME, HOSTS_FILENAME, RESOLV_FILENAME, SHARED_DIR_SUFFIX,
};

use crate::{
    cgroup::SandboxCgroup,
    client::{
        client_check, client_sync_clock, client_update_interfaces, client_update_routes,
        new_sandbox_client,
    },
    container::KuasarContainer,
    network::{Network, NetworkConfig},
    utils::{get_dns_config, get_hostname, get_resources, get_sandbox_cgroup_parent_path},
    vm::{Hooks, Recoverable, VMFactory, VM},
};

pub const KUASAR_GUEST_SHARE_DIR: &str = "/run/kuasar/storage/containers/";

macro_rules! _monitor {
    ($sb:ident) => {
        tokio::spawn(async move {
            let mut rx = {
                let sandbox = $sb.lock().await;
                if let SandboxStatus::Running(_) = sandbox.status.clone() {
                    sandbox.vm.wait_channel().await.unwrap()
                } else {
                    error!("can not get wait channel when sandbox is running");
                    return;
                }
            };

            let (code, ts) = *rx.borrow();
            if ts == 0 {
                rx.changed().await.unwrap_or_default();
                let (code, ts) = *rx.borrow();
                let mut sandbox = $sb.lock().await;
                sandbox.status = SandboxStatus::Stopped(code, ts);
                sandbox.exit_signal.signal();
            } else {
                let mut sandbox = $sb.lock().await;
                sandbox.status = SandboxStatus::Stopped(code, ts);
                sandbox.exit_signal.signal();
            }
        });
    };
}

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

    pub fn log_level(&self) -> &str {
        &self.config.log_level
    }
}

impl<F, H> KuasarSandboxer<F, H>
where
    F: VMFactory,
    H: Hooks<F::VM>,
    F::VM: VM + DeserializeOwned + Recoverable + Sync + Send + 'static,
{
    pub async fn recover(&mut self, dir: &str) -> Result<()> {
        let mut subs = tokio::fs::read_dir(dir).await.map_err(Error::IO)?;
        while let Some(entry) = subs.next_entry().await.unwrap() {
            if let Ok(t) = entry.file_type().await {
                if t.is_dir() {
                    let path = Path::new(dir).join(entry.file_name());
                    match KuasarSandbox::recover(&path).await {
                        Ok(sb) => {
                            let sb_mutex = Arc::new(Mutex::new(sb));
                            let sb_clone = sb_mutex.clone();
                            monitor(sb_clone);
                            self.sandboxes
                                .write()
                                .await
                                .insert(entry.file_name().to_str().unwrap().to_string(), sb_mutex);
                        }
                        Err(e) => {
                            warn!("failed to recover sandbox {:?}, {:?}", entry.file_name(), e);
                            cleanup_mounts(path.to_str().unwrap()).await?;
                            remove_dir_all(&path).await?
                        }
                    }
                }
            }
        }
        Ok(())
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

    async fn create(&self, id: &str, s: SandboxOption) -> Result<()> {
        if self.sandboxes.read().await.get(id).is_some() {
            return Err(Error::AlreadyExist("sandbox".to_string()));
        }

        let mut sandbox_cgroups = SandboxCgroup::default();
        let cgroup_parent_path = match get_sandbox_cgroup_parent_path(&s.sandbox) {
            Some(cgroup_parent_path) => cgroup_parent_path,
            None => {
                return Err(Error::Other(anyhow!(
                    "Failed to get sandbox cgroup parent path."
                )))
            }
        };
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

        // TODO support network
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

        // Handle pod network if it has a private network namespace
        if !s.sandbox.netns.is_empty() {
            // get vcpu for interface queue
            let mut vcpu = 1;
            if let Some(resources) = get_resources(&s.sandbox) {
                if resources.cpu_period > 0 && resources.cpu_quota > 0 {
                    // get ceil of cpus if it is not integer
                    let base = (resources.cpu_quota as f64 / resources.cpu_period as f64).ceil();
                    vcpu = base as u32;
                }
            }

            let network_config = NetworkConfig {
                netns: s.sandbox.netns.to_string(),
                sandbox_id: id.to_string(),
                queue: vcpu,
            };
            let network = Network::new(network_config).await?;
            network.attach_to(&mut sandbox).await?;
        }

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

    async fn start(&self, id: &str) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_start(&mut sandbox).await?;
        sandbox.start().await?;

        // Currently only support cgroup V1, cgroup V2 is not supported now
        if !cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            // add vmm process into sandbox cgroup
            if let SandboxStatus::Running(vmm_pid) = sandbox.status {
                let vcpu_threads = sandbox.vm.vcpus().await?;
                debug!(
                    "vmm process pid: {}, vcpu threads pid: {:?}",
                    vmm_pid, vcpu_threads
                );
                sandbox
                    .sandbox_cgroups
                    .add_process_into_sandbox_cgroups(vmm_pid, Some(vcpu_threads))?;
                // move all vmm-related process into sandbox cgroup
                for pid in sandbox.vm.pids().affilicated_pids {
                    sandbox
                        .sandbox_cgroups
                        .add_process_into_sandbox_cgroups(pid, None)?;
                }
            } else {
                return Err(Error::Other(anyhow!(
                    "sandbox status is not Running after started!"
                )));
            }
        }

        let sandbox_clone = sandbox_mutex.clone();
        monitor(sandbox_clone);
        self.hooks.post_start(&mut sandbox).await?;
        sandbox.dump().await?;
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

    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let sandbox_mutex = self.sandbox(id).await?;
        let mut sandbox = sandbox_mutex.lock().await;
        self.hooks.pre_stop(&mut sandbox).await?;
        sandbox.stop(force).await?;
        self.hooks.post_stop(&mut sandbox).await?;
        sandbox.dump().await?;
        Ok(())
    }

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
            remove_dir_all(&sb.base_dir).await?;
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

    fn status(&self) -> Result<SandboxStatus> {
        Ok(self.status.clone())
    }

    async fn ping(&self) -> Result<()> {
        self.vm.ping().await
    }

    async fn container(&self, id: &str) -> Result<&Self::Container> {
        let container = self
            .containers
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        Ok(container)
    }

    async fn append_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_append_handlers(id, options)?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

    async fn update_container(&mut self, id: &str, options: ContainerOption) -> Result<()> {
        let handler_chain = self.container_update_handlers(id, options).await?;
        handler_chain.handle(self).await?;
        self.dump().await?;
        Ok(())
    }

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

    async fn exit_signal(&self) -> Result<Arc<ExitSignal>> {
        Ok(self.exit_signal.clone())
    }

    fn get_data(&self) -> Result<SandboxData> {
        Ok(self.data.clone())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
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

impl<V> KuasarSandbox<V>
where
    V: VM + DeserializeOwned + Recoverable + Sync + Send,
{
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
            sb.vm.recover().await?;
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
    async fn start(&mut self) -> Result<()> {
        let pid = self.vm.start().await?;

        if let Err(e) = self.init_client().await {
            self.vm.stop(true).await.unwrap_or_default();
            return Err(e);
        }

        if let Err(e) = self.setup_network().await {
            self.vm.stop(true).await.unwrap_or_default();
            return Err(e);
        }

        self.status = SandboxStatus::Running(pid);
        Ok(())
    }

    async fn stop(&mut self, force: bool) -> Result<()> {
        match self.status {
            // If sandbox is created but not running, no need to stop.
            SandboxStatus::Created => {
                return Ok(());
            }
            SandboxStatus::Running(_) => {}
            SandboxStatus::Stopped(_, _) => {
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
        if let Some(network) = self.network.as_mut() {
            network.destroy().await;
        }
        Ok(())
    }

    pub(crate) fn container_mut(&mut self, id: &str) -> Result<&mut KuasarContainer> {
        self.containers
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("no container with id {}", id)))
    }

    pub(crate) fn increment_and_get_id(&mut self) -> u32 {
        self.id_generator += 1;
        self.id_generator
    }

    async fn init_client(&mut self) -> Result<()> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let addr = self.vm.socket_address();
            if addr.is_empty() {
                return Err(anyhow!("VM address is empty").into());
            }
            let client = new_sandbox_client(&addr).await?;
            client_check(&client).await?;
            *client_guard = Some(client)
        }
        Ok(())
    }

    pub(crate) async fn setup_network(&mut self) -> Result<()> {
        if let Some(network) = self.network.as_ref() {
            let client_guard = self.client.lock().await;
            if let Some(client) = &*client_guard {
                client_update_interfaces(client, network.interfaces()).await?;
                client_update_routes(client, network.routes()).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn sync_clock(&self) {
        let client_guard = self.client.lock().await;
        if let Some(client) = &*client_guard {
            client_sync_clock(client, self.id.as_str()).await;
        }
    }

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

    pub fn get_sandbox_shared_path(&self) -> String {
        format!("{}/{}", self.base_dir, SHARED_DIR_SUFFIX)
    }
}

// parse_dnsoptions parse DNS options into resolv.conf format content,
// if none option is specified, will return empty with no error.
fn parse_dnsoptions(
    servers: &Vec<String>,
    searches: &Vec<String>,
    options: &Vec<String>,
) -> String {
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

#[derive(Default, Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub log_level: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDeviceSpec {
    #[serde(default)]
    pub(crate) _host_path: Vec<String>,
    #[serde(default)]
    pub(crate) _bdf: Vec<String>,
    #[allow(dead_code)]
    #[deprecated]
    #[serde(default)]
    pub(crate) gpu_group_id: i32,
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
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
        } else {
            let mut sandbox = sandbox_mutex.lock().await;
            sandbox.status = SandboxStatus::Stopped(code, ts);
            sandbox.exit_signal.signal();
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
            let mut dns_test = DnsConfig::default();
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
