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

use std::path::Path;

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::{
    error::Error,
    spec::{JsonSpec, Mount},
};
use path_clean::clean;
use vmm_common::{
    ETC_HOSTNAME, ETC_HOSTS, ETC_RESOLV, HOSTNAME_FILENAME, HOSTS_FILENAME, KUASAR_STATE_DIR,
    RESOLV_FILENAME,
};

use crate::{
    container::handler::Handler, sandbox::KuasarSandbox, utils::write_file_atomic, vm::VM,
};

const CONFIG_FILE_NAME: &str = "config.json";

pub struct SpecHandler {
    container_id: String,
}

impl SpecHandler {
    pub fn new(container_id: &str) -> Self {
        Self {
            container_id: container_id.to_string(),
        }
    }
}

#[async_trait]
impl<T> Handler<KuasarSandbox<T>> for SpecHandler
where
    T: VM + Sync + Send,
{
    async fn handle(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        let shared_path = sandbox.get_sandbox_shared_path();
        let container = sandbox.container_mut(&self.container_id)?;
        let spec = container
            .data
            .spec
            .as_mut()
            .ok_or(Error::InvalidArgument(format!(
                "no spec for container {}",
                self.container_id
            )))?;
        // TODO support apparmor in guest os, but not remove the apparmor profile here
        if let Some(p) = spec.process.as_mut() {
            p.apparmor_profile = "".to_string();
        }
        // Update sandbox files mounts for container
        container_mounts(&shared_path, spec);
        let spec_str = serde_json::to_string(spec)
            .map_err(|e| anyhow!("failed to parse spec in sandbox, {}", e))?;
        let config_path = format!("{}/{}", container.data.bundle, CONFIG_FILE_NAME);
        write_file_atomic(config_path, &spec_str).await?;
        Ok(())
    }

    async fn rollback(
        &self,
        sandbox: &mut KuasarSandbox<T>,
    ) -> containerd_sandbox::error::Result<()> {
        let bundle = format!(
            "{}/{}",
            sandbox.get_sandbox_shared_path(),
            self.container_id
        );
        tokio::fs::remove_dir_all(&*bundle)
            .await
            .map_err(|e| anyhow!("failed to remove container bundle, {}", e))?;
        Ok(())
    }
}

// container_mounts sets up necessary container system file mounts
// including /etc/hostname, /etc/hosts and /etc/resolv.conf.
fn container_mounts(shared_path: &str, spec: &mut JsonSpec) {
    let rw_option = if spec.root.as_ref().map(|r| r.readonly).unwrap_or_default() {
        "ro"
    } else {
        "rw"
    };

    let mut extra_mounts: Vec<Mount> = vec![];
    let cri_mount_handler = |dst, filename, extra_mounts: &mut Vec<Mount>| {
        if !is_in_cri_mounts(dst, &spec.mounts) {
            let host_path = format!("{}/{}", shared_path, filename);
            // If host path exist, should add it to container mount
            if Path::exists(host_path.as_ref()) {
                extra_mounts.push(Mount {
                    destination: dst.to_string(),
                    r#type: "bind".to_string(),
                    source: format!("{}/{}", KUASAR_STATE_DIR, filename),
                    options: vec!["rbind", "rprivate", rw_option]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                });
            }
        }
    };

    cri_mount_handler(ETC_HOSTNAME, HOSTNAME_FILENAME, &mut extra_mounts);
    cri_mount_handler(ETC_HOSTS, HOSTS_FILENAME, &mut extra_mounts);
    cri_mount_handler(ETC_RESOLV, RESOLV_FILENAME, &mut extra_mounts);
    spec.mounts.append(&mut extra_mounts);
}

fn is_in_cri_mounts(dst: &str, mounts: &Vec<Mount>) -> bool {
    for mount in mounts {
        if clean(&mount.destination) == clean(dst) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use containerd_sandbox::spec::{JsonSpec, Mount, Root};
    use containerd_shim::util::write_str_to_file;
    use temp_dir::TempDir;

    use crate::container::handler::spec::{container_mounts, is_in_cri_mounts};

    fn generate_cri_mounts() -> Vec<Mount> {
        vec![
            Mount {
                destination: "/etc/hosts".to_string(),
                r#type: "".to_string(),
                source: "/run/kuasar-vmm/0d042b22dbaf083f704c5945488c7f63d10a664f743802a7901ac6fae6460d9b/shared/hosts".to_string(),
                options: vec![],
            },
            Mount {
                destination: "/etc/hostname".to_string(),
                r#type: "".to_string(),
                source: "/run/kuasar-vmm/0d042b22dbaf083f704c5945488c7f63d10a664f743802a7901ac6fae6460d9b/shared/hostname".to_string(),
                options: vec![],
            },
            Mount {
                destination: "/etc/resolv.conf".to_string(),
                r#type: "".to_string(),
                source: "/run/kuasar-vmm/0d042b22dbaf083f704c5945488c7f63d10a664f743802a7901ac6fae6460d9b/shared/resolv.conf".to_string(),
                options: vec![],
            },
            Mount {
                destination: "/dev/shm".to_string(),
                r#type: "".to_string(),
                source: "/run/kuasar-vmm/0d042b22dbaf083f704c5945488c7f63d10a664f743802a7901ac6fae6460d9b/shared/shm".to_string(),
                options: vec![],
            },
        ]
    }

    #[test]
    fn test_is_in_cri_mounts() {
        let cri_mounts = generate_cri_mounts();
        assert!(is_in_cri_mounts("/etc/hostname", &cri_mounts));
        assert!(is_in_cri_mounts("/etc/hosts", &cri_mounts));
        assert!(is_in_cri_mounts("/etc/resolv.conf", &cri_mounts));
        assert!(is_in_cri_mounts("/dev/shm", &cri_mounts));
        assert!(!is_in_cri_mounts("/var/lib/kuasar", &cri_mounts));
    }

    #[tokio::test]
    // When no mount defined in spec and kuasar doesn't create these files, expect no mount added.
    async fn test_container_mounts_with_target_file_not_exist() {
        let mut spec = JsonSpec::default();
        spec.mounts = vec![];

        let tmp_path = TempDir::new().unwrap();
        let shared_path = tmp_path.path().to_str().unwrap();
        container_mounts(shared_path, &mut spec);
        assert_eq!(spec.mounts.len(), 0);
    }

    #[tokio::test]
    // When no mount defined in spec and kuasar created hostname file, expect hostname mount is added.
    async fn test_container_mounts_with_hostname_file_exist() {
        let mut spec = JsonSpec::default();
        spec.mounts = vec![];
        spec.root = Some(Root::default());
        assert!(!spec.root.clone().expect("root should not be None").readonly);

        let tmp_path = TempDir::new().unwrap();
        let shared_path = tmp_path.path().to_str().unwrap();
        write_str_to_file(tmp_path.child("hostname"), "kuasar-deno-001")
            .await
            .unwrap();
        container_mounts(shared_path, &mut spec);
        assert_eq!(spec.mounts.len(), 1);
        assert!(spec.mounts[0].options.contains(&"rw".to_string()));
        let parent_path = Path::new(&spec.mounts[0].source)
            .parent()
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(parent_path, "/run/kuasar/state");
    }

    #[tokio::test]
    // When readonly rootfs defined in spec, expect hostname mount is readonly.
    async fn test_container_mounts_with_mounts_and_ro() {
        let mut spec = JsonSpec::default();
        spec.mounts = vec![];
        spec.root = Some(Root {
            path: "".to_string(),
            readonly: true,
        });
        assert!(spec.root.clone().expect("root should not be None").readonly);

        let tmp_path = TempDir::new().unwrap();
        let shared_path = tmp_path.path().to_str().unwrap();
        write_str_to_file(tmp_path.child("hostname"), "kuasar-deno-001")
            .await
            .unwrap();
        container_mounts(shared_path, &mut spec);
        assert_eq!(spec.mounts.len(), 1);
        assert!(spec.mounts[0].options.contains(&"ro".to_string()));
    }

    #[tokio::test]
    // When hostname mount already defined, expect hostname mount is no changed.
    async fn test_container_mounts_with_mount_predefined() {
        let mut spec = JsonSpec::default();
        let cri_mount = generate_cri_mounts();
        spec.mounts = cri_mount.clone();

        let tmp_path = TempDir::new().unwrap();
        let shared_path = tmp_path.path().to_str().unwrap();
        container_mounts(shared_path, &mut spec);
        assert_eq!(spec.mounts.len(), 4);
        assert_eq!(cri_mount[0].source, spec.mounts[0].source);
        assert_eq!(cri_mount[0].r#type, spec.mounts[0].r#type);
        assert_eq!(cri_mount[0].destination, spec.mounts[0].destination);
        assert_eq!(cri_mount[0].options, spec.mounts[0].options);
    }
}
