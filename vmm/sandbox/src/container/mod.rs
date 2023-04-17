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

use containerd_sandbox::{
    data::{ContainerData, ProcessData},
    error, Container,
};
use serde::{Deserialize, Serialize};

mod handler;

#[allow(dead_code)]
const NULL_DEVICE: &str = "/dev/null";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KuasarContainer {
    #[allow(dead_code)]
    pub(crate) id: String,
    pub(crate) data: ContainerData,
    pub(crate) io_devices: Vec<String>,
    pub(crate) processes: Vec<KuasarProcess>,
}

impl Container for KuasarContainer {
    fn get_data(&self) -> error::Result<ContainerData> {
        let mut data = self.data.clone();
        data.processes = self.processes.iter().map(|x| x.data.clone()).collect();
        Ok(data)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KuasarProcess {
    pub id: String,
    pub io_devices: Vec<String>,
    pub data: ProcessData,
}

impl KuasarProcess {
    pub fn new(data: ProcessData) -> Self {
        Self {
            id: data.id.to_string(),
            io_devices: vec![],
            data,
        }
    }
}
