/*
Copyright 2024 The Kuasar Authors.

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

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use lazy_static::lazy_static;
use log::{debug, error, info};
use containerd_shim::{
    Error, Result,
    other, other_error
};
use image_rs::{image::ImageClient, bundle::BUNDLE_ROOTFS};
use tokio::sync::Mutex;
use oci_spec::runtime::{Process, Spec};

use crate::config::TaskConfig;

// A marker to merge container spec for images pulled inside guest.
pub const ANNO_K8S_IMAGE_NAME: &str = "io.kubernetes.cri.image-name";

// Kuasar rootfs is readonly, use tmpfs before CC storage is implemented.
const KUASAR_CC_IMAGE_WORK_DIR: &str = "/run/kuasar/image/";
const CONFIG_JSON: &str = "config.json";

pub const CONTAINER_BASE: &str = "/run/kuasar-vmm";

lazy_static! {
    pub static ref KUASAR_IMAGE_CLIENT: Mutex<Option<KuasarImageClient>> = Mutex::new(None);
}

#[derive(Clone)]
pub struct KuasarImageClient {
    image_client: Arc<Mutex<ImageClient>>,
    images: Arc<Mutex<HashMap<String, String>>>,
    aa_kbc_params: String,
    https_proxy: String,
    no_proxy: String,
}

pub struct ProxyDrop {
    _noop: i32,
}

impl Drop for ProxyDrop {
    /// Unset proxy environment
    fn drop(&mut self) {
        debug!("unset proxy");
        env::set_var("HTTPS_PROXY", "".to_string());
        env::set_var("NO_PROXY", "".to_string());
    }
}

impl KuasarImageClient {
    pub fn new(task_config: &TaskConfig) -> Self {
        let mut image_client = ImageClient::default();

        if !task_config.image_config.image_policy_file.is_empty() {
            image_client.config.file_paths.policy_path = task_config.image_config.image_policy_file.clone();
        }
        if !task_config.image_config.simple_signing_sigstore_config.is_empty() {
            image_client.config.file_paths.sigstore_config =
                task_config.image_config.simple_signing_sigstore_config.clone();
        }
        if !task_config.image_config.image_registry_auth_file.is_empty() {
            image_client.config.file_paths.auth_file =
                task_config.image_config.image_registry_auth_file.clone();
        }
        image_client.config.work_dir =
            std::path::PathBuf::from(KUASAR_CC_IMAGE_WORK_DIR.to_string());

        let aa_kbc_params = &task_config.image_config.aa_kbc_params;
        // If the attestation-agent is being used, then enable the authenticated credentials support
        info!("image_client.config.auth set to: {}", !aa_kbc_params.is_empty());
        image_client.config.auth = !aa_kbc_params.is_empty();
        
        // Check if signature verification is enabled, and set it in image client.
        let enable_signature_verification = &task_config.image_config.enable_signature_verification;
        info!("enable_signature_verification set to: {}", enable_signature_verification);
        image_client.config.security_validate = *enable_signature_verification;

        Self {
            image_client: Arc::new(Mutex::new(image_client)),
            images: Arc::new(Mutex::new(HashMap::new())),
            https_proxy: task_config.image_config.https_proxy.clone(),
            no_proxy: task_config.image_config.no_proxy.clone(),
            aa_kbc_params: task_config.image_config.aa_kbc_params.clone(),
        }
    }

    /// Get the singleton instance of image service.
    pub async fn singleton() -> Result<KuasarImageClient> {
        let client = KUASAR_IMAGE_CLIENT.lock().await.clone();
        match client {
            Some(client) => Ok(client),
            None => Err(other!("image service is uninitialized")),
        }
    }

    /// Set proxy environment
    fn set_proxy_env_vars(&self) -> ProxyDrop {
        let https_proxy = self.https_proxy.clone();
        if !https_proxy.is_empty() {
            env::set_var("HTTPS_PROXY", https_proxy);
        }
        let no_proxy = self.no_proxy.clone();
        if !no_proxy.is_empty() {
            env::set_var("NO_PROXY", no_proxy);
        }
        ProxyDrop {
            _noop: 0
        }
    }

    /// init decrypt config
    async fn get_security_config(&self) -> Result<String> {
        let aa_kbc_params = self.aa_kbc_params.clone();
        let decrypt_config = if !aa_kbc_params.is_empty() {
            format!("provider:attestation-agent:{}", aa_kbc_params)
        } else {
            "".to_string()
        };

        Ok(decrypt_config)
    }

    /// Call image-rs to pull and unpack image.
    async fn common_image_pull(
        &self,
        image: &str,
        bundle_path: &Path,
        decrypt_config: &str,
        source_creds: Option<&str>,
        cid: &str,
    ) -> Result<()> {
        let res = self
            .image_client
            .lock()
            .await
            .pull_image(image, bundle_path, &source_creds, &Some(decrypt_config))
            .await;
        match res {
            Ok(image) => {
                info!("Successfully pull and unpack image {:?}, cid: {:?}, with image-rs. ", image, cid);
            }
            Err(e) => {
                error!("Failed to pull and unpack image {:?}, cid: {:?}, with image-rs {:?}. ",
                    image, cid, e.to_string());
                return Err(other!("Failed to pull and unpack image {:?}, cid: {:?}, with image-rs {:?}. ",
                    image, cid, e.to_string()));
            }
        };
        self.add_image(String::from(image), String::from(cid)).await;
        Ok(())
    }

    /// Pull image when creating container and return the bundle path with rootfs.
    pub async fn pull_image_for_container(
        &self,
        image: &str,
        cid: &str,
    ) -> Result<String> {
        let _proxy_drop = self.set_proxy_env_vars();
        
        let bundle_path = Path::new(CONTAINER_BASE).join(cid);
        fs::create_dir_all(&bundle_path).or_else(|e| { Err(other!("failed to create dir {:?} {:?}", &bundle_path, e)) })?;
        info!("pull image {:?}, bundle path {:?}", cid, bundle_path);

        let decrypt_config = self.get_security_config().await?;

        let source_creds = None; // You need to determine how to obtain this.

        self.common_image_pull(image, &bundle_path, &decrypt_config, source_creds, cid)
            .await?;
        Ok(format!{"{}/{}", bundle_path.display(), BUNDLE_ROOTFS})
    }

    async fn add_image(&self, image: String, cid: String) {
        self.images.lock().await.insert(image, cid);
    }

    // When being passed an image name through a container annotation, merge its
    // corresponding bundle OCI specification into the passed container creation one.
    pub async fn merge_bundle_oci(&self, container_oci: &mut Spec) -> Result<()> {
        let annotations = container_oci.annotations().clone().unwrap_or_default();
        if let Some(image_name) = annotations.get(&ANNO_K8S_IMAGE_NAME.to_string())
        {
            let images = self.images.lock().await;
            if let Some(container_id) = images.get(image_name) {
                let image_oci_config_path = Path::new(CONTAINER_BASE)
                    .join(container_id)
                    .join(CONFIG_JSON);
                debug!("image bundle config path: {:?}", image_oci_config_path);

                let image_oci =
                    Spec::load(image_oci_config_path.to_str().ok_or_else(|| 
                        other!("invalid container image OCI config path {:?}", image_oci_config_path)
                    )?).unwrap_or_default();

                if let Some(mut container_root) = container_oci.root().clone() {
                    if let Some(image_root) = image_oci.root() {
                        let root_path = Path::new(CONTAINER_BASE)
                            .join(container_id)
                            .join(image_root.path().clone());
                        container_root.set_path(
                            std::path::PathBuf::from(
                                &String::from(root_path.to_str().ok_or_else(||
                                    other!("invalid container image root path {:?}", root_path)
                                )?)
                            )
                        );
                    }
                }

                if let Some(mut container_process) = container_oci.process().clone() {
                    if let Some(image_process) = image_oci.process() {
                        self.merge_oci_process(&mut container_process, image_process);
                    }
                }
            }
        }

        Ok(())
    }

    // Partially merge an OCI process specification into another one.
    fn merge_oci_process(&self, target: &mut Process, source: &Process) {
        if target.args().is_none() && !source.args().is_none() {
            target.set_args(source.args().clone());
        }

        if target.cwd().to_str() == Some("/") && source.cwd().to_str() != Some("/") {
            target.set_cwd(source.cwd().clone());
        }

        if let Some(mut env) = target.env().clone() {
            for source_env in source.env().clone().unwrap_or_default() {
                let variable_name: Vec<&str> = source_env.split('=').collect();
                if !target.env().iter().any(|i| i.contains(&variable_name[0].to_string())) {
                    env.push(source_env.to_string());
                }
            }
            target.set_env(Some(env));
        }
    }

    // Image-rs:pull_image will generate image config everytime,
    // so we need del image config after container stopped.
    pub async fn del_image_config(&self, cid: &str) -> Result<()> {
        let image_oci_config_path = Path::new(CONTAINER_BASE)
            .join(cid)
            .join(CONFIG_JSON);
        fs::remove_file(&image_oci_config_path).map_err(other_error!(e, "remove image config"))?;
        debug!("del image config: {:?}", &image_oci_config_path);
        Ok(())
    }
    
}
