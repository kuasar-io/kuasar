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

use oci_spec::runtime::Spec;

#[cfg(feature = "wasmedge")]
pub fn get_preopens(spec: &Spec) -> Vec<String> {
    let mut preopens = vec![];
    if let Some(mounts) = spec.mounts() {
        for m in mounts {
            if let Some(typ) = m.typ() {
                if typ == "bind" && m.source().is_some() {
                    preopens.push(format!(
                        "{}:{}",
                        m.destination().display(),
                        m.source().as_ref().unwrap().display()
                    ));
                }
            }
        }
    }

    preopens
}

#[cfg(feature = "wasmedge")]
pub fn get_envs(spec: &Spec) -> Vec<String> {
    let empty_envs = vec![];
    let envs = spec
        .process()
        .as_ref()
        .unwrap()
        .env()
        .as_ref()
        .unwrap_or(&empty_envs);
    envs.to_vec()
}

#[cfg(feature = "wasmtime")]
pub fn get_kv_envs(spec: &Spec) -> Vec<(String, String)> {
    let empty_envs = vec![];
    let envs = spec
        .process()
        .as_ref()
        .unwrap()
        .env()
        .as_ref()
        .map(|x| {
            x.iter()
                .map(|e| {
                    e.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .unwrap_or((e.to_string(), "".to_string()))
                })
                .collect::<Vec<(String, String)>>()
        })
        .unwrap_or(empty_envs);
    envs.to_vec()
}

pub fn get_args(spec: &Spec) -> Vec<String> {
    let empty_args = vec![];
    let args = spec
        .process()
        .as_ref()
        .unwrap()
        .args()
        .as_ref()
        .unwrap_or(&empty_args);
    args.to_vec()
}

#[cfg(feature = "wasmtime")]
pub fn get_memory_limit(spec: &Spec) -> Option<i64> {
    spec.linux()
        .as_ref()
        .and_then(|x| x.resources().as_ref())
        .and_then(|x| x.memory().as_ref())
        .and_then(|x| x.limit())
}

pub(crate) fn get_rootfs(spec: &Spec) -> Option<String> {
    spec.root()
        .as_ref()
        .map(|root| root.path().display().to_string())
}

#[cfg(feature = "wasmedge")]
pub(crate) fn get_cgroup_path(spec: &Spec) -> Option<String> {
    spec.linux()
        .as_ref()
        .and_then(|linux| linux.cgroups_path().as_ref())
        .map(|path| path.display().to_string())
}
