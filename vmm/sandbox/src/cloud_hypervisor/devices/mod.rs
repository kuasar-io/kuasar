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

use serde_derive::{Deserialize, Serialize};

use crate::{device::Device, param::ToCmdLineParams};

pub mod block;
pub mod console;
pub mod device;
pub mod fs;
pub mod pmem;
pub mod rng;
pub mod vfio;
pub mod virtio_net;
pub mod vsock;

pub trait CloudHypervisorDevice: Device + ToCmdLineParams + Sync + Send {}

impl<T> CloudHypervisorDevice for T where T: Device + ToCmdLineParams + Sync + Send {}

#[derive(Deserialize, Debug)]
pub struct AddDeviceResponse {
    pub id: String,
    pub bdf: String,
}

#[derive(Serialize, Debug)]
pub struct RemoveDeviceRequest {
    pub id: String,
}
