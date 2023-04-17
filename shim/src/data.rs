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

use containerd_shim::{
    error::{Error, Result},
    other,
};

use crate::io::ContainerIoTransport;

// Only care about data that is really used in shim,
// no matter where it from, sandbox, container or process

#[derive(Clone, Default)]
pub struct ProcessData<T> {
    pub id: String,
    pub io: T,
}

impl<T> ProcessData<T> {
    pub fn new(id: &str, io: T) -> Self {
        Self {
            id: id.to_string(),
            // process_spec: None,
            io,
        }
    }
}

#[derive(Clone, Default)]
pub struct ContainerData<T> {
    pub id: String,
    pub io: T,
    pub processes: Vec<ProcessData<T>>,
}

impl<T: ContainerIoTransport> ContainerData<T> {
    pub fn new(id: &str, io: T) -> Self {
        Self {
            id: id.to_string(),
            io,
            processes: vec![],
        }
    }

    pub fn add_process_data(&mut self, process_data: ProcessData<T>) {
        self.processes.push(process_data);
    }

    pub fn get_process_data(&self, exec_id: &str) -> Result<&ProcessData<T>> {
        self.processes
            .iter()
            .find(|p| p.id == exec_id)
            .ok_or_else(|| other!("can't get process by exec_id {}", exec_id))
    }

    pub fn delete_process_data(&mut self, exec_id: &str) {
        self.processes.retain(|p| p.id != exec_id);
    }
}

#[derive(Clone, Default)]
pub struct SandboxData<T> {
    pub containers: Vec<ContainerData<T>>,
}

impl<T> SandboxData<T> {
    pub fn add_container_data(&mut self, container_data: ContainerData<T>) {
        self.containers.push(container_data);
    }

    pub fn get_container_data(&self, id: &str) -> Result<&ContainerData<T>> {
        self.containers
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| other!("can't get container by id {}", id))
    }

    pub fn get_mut_container_data(&mut self, id: &str) -> Result<&mut ContainerData<T>> {
        self.containers
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| other!("can't get container by id {}", id))
    }

    pub fn delete_container_data(&mut self, id: &str) {
        self.containers.retain(|c| c.id != id);
    }
}
