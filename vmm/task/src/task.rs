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

use std::sync::Arc;

use containerd_shim::{
    asynchronous::{
        container::Container,
        monitor::{monitor_subscribe, monitor_unsubscribe, Subscription},
        processes::Process,
        task::TaskService,
        util::read_spec,
    },
    monitor::{Subject, Topic},
};
use log::{debug, error};
use oci_spec::runtime::{LinuxNamespaceType, Spec};
use tokio::sync::{mpsc::channel, Mutex};

use crate::{
    container::{KuasarContainer, KuasarFactory},
    sandbox::SandboxResources,
};

pub(crate) async fn create_task_service() -> TaskService<KuasarFactory, KuasarContainer> {
    let (tx, mut rx) = channel(128);
    let sandbox = Arc::new(Mutex::new(SandboxResources::new().await));
    let task = TaskService {
        factory: KuasarFactory::new(sandbox),
        containers: Arc::new(Default::default()),
        namespace: "k8s.io".to_string(),
        exit: Arc::new(Default::default()),
        tx: tx.clone(),
    };
    let s = monitor_subscribe(Topic::Pid)
        .await
        .expect("monitor subscribe failed");
    process_exits(s, &task).await;
    tokio::spawn(async move {
        while let Some((_topic, e)) = rx.recv().await {
            debug!("received event {:?}", e);
        }
    });
    task
}

async fn process_exits(s: Subscription, task: &TaskService<KuasarFactory, KuasarContainer>) {
    let containers = task.containers.clone();
    let mut s = s;
    tokio::spawn(async move {
        while let Some(e) = s.rx.recv().await {
            if let Subject::Pid(pid) = e.subject {
                debug!("receive exit event: {}", &e);
                let exit_code = e.exit_code;
                for (_k, cont) in containers.lock().await.iter_mut() {
                    let bundle = cont.bundle.to_string();
                    // pid belongs to container init process
                    if cont.init.pid == pid {
                        // kill all children process if the container has a private PID namespace
                        if should_kill_all_on_exit(&bundle).await {
                            cont.kill(None, 9, true).await.unwrap_or_else(|e| {
                                error!("failed to kill init's children: {}", e)
                            });
                        }
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
        }
        monitor_unsubscribe(s.id).await.unwrap_or_default();
    });
}

async fn should_kill_all_on_exit(bundle_path: &str) -> bool {
    match read_spec(bundle_path).await {
        Ok(spec) => has_shared_pid_namespace(&spec),
        Err(e) => {
            error!(
                "failed to read spec when call should_kill_all_on_exit: {}",
                e
            );
            true
        }
    }
}

pub fn has_shared_pid_namespace(spec: &Spec) -> bool {
    match spec.linux() {
        None => true,
        Some(linux) => match linux.namespaces() {
            None => true,
            Some(namespaces) => {
                for ns in namespaces {
                    if ns.typ() == LinuxNamespaceType::Pid && ns.path().is_none() {
                        return false;
                    }
                }
                true
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::task::should_kill_all_on_exit;

    #[tokio::test]
    async fn test_should_kill_all_on_exit_when_no_path() {
        let path = "fake-path";
        // fake-path should not exist
        assert!(!Path::new(path).exists());

        // return true if path not exist
        assert!(should_kill_all_on_exit(path).await);
    }
}
