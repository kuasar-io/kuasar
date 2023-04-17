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

pub trait GetAndSetDeviceAddr {
    fn get_device_addr(&self) -> String;
    fn set_device_addr(&mut self, addr: usize);
}

macro_rules! impl_set_get_device_addr {
    ($ty:ty) => {
        impl crate::stratovirt::devices::device::GetAndSetDeviceAddr for $ty {
            fn get_device_addr(&self) -> String {
                self.addr.clone()
            }

            fn set_device_addr(&mut self, addr: usize) {
                self.addr = format!("{:#02x}", addr);
            }
        }
    };
}
