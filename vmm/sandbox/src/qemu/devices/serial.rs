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

use sandbox_derive::CmdLineParams;

use crate::device::{Bus, BusType, Slot, Transport};

const VIRTIO_SERIAL_DRIVER: &str = "virtio-serial";
const SERIAL_BUS_CAPACITY: usize = 31;

#[derive(CmdLineParams, Debug, Clone)]
#[params("device")]
pub struct SerialBridge {
    #[property(ignore_key)]
    pub driver: String,
    pub id: String,
    pub disable_modern: Option<bool>,
    pub romfile: Option<String>,
    #[property(key = "max_ports")]
    pub max_ports: Option<i32>,
    #[property(ignore)]
    pub transport: Transport,

    #[property(ignore)]
    pub(crate) bus: Bus,
}

impl SerialBridge {
    pub fn new(id: &str, transport: Transport) -> Self {
        SerialBridge {
            driver: transport.to_driver(VIRTIO_SERIAL_DRIVER),
            id: id.to_string(),
            disable_modern: transport.disable_modern(false),
            romfile: None,
            max_ports: None,
            transport,
            bus: Bus {
                r#type: BusType::SERIAL,
                id: id.to_string(),
                bus_addr: format!("{}.0", id),
                slots: vec![Slot::default(); SERIAL_BUS_CAPACITY],
            },
        }
    }
}

impl_device!(SerialBridge);
