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

#[derive(CmdLineParams, Debug, Clone)]
#[params("chardev")]
pub struct CharDevice {
    #[property(ignore_key)]
    pub backend: String,
    pub id: String,
    pub path: String,
    #[property(
        param = "chardev",
        ignore_key,
        predicate = "self.backend == \"socket\"",
        generator = "crate::utils::bool_to_socket_server"
    )]
    pub server: bool,
    #[property(
        param = "chardev",
        ignore_key,
        predicate = "self.backend == \"socket\"",
        generator = "crate::utils::bool_to_socket_nowait"
    )]
    pub nowait: bool,
    #[property(ignore)]
    pub addr: String,
}

impl_device_no_bus!(CharDevice);
impl_set_get_device_addr!(CharDevice);

impl CharDevice {
    pub fn new(backend: &str, chardev_id: &str, path: &str) -> Self {
        Self {
            backend: backend.to_string(),
            id: chardev_id.to_string(),
            path: path.to_string(),
            server: true,
            nowait: true,
            addr: "".to_string(),
        }
    }
}

mod tests {
    #[allow(unused_imports)]
    use super::CharDevice;
    #[allow(unused_imports)]
    use crate::param::ToCmdLineParams;

    #[test]
    fn test_char_device_params() {
        let char_device = CharDevice::new(
            "socket",
            "charconsole0",
            "/run/vc/vm/sandbox-id/console.sock",
        );
        let char_device_cmd_params = char_device.to_cmdline_params("-");
        println!("chardev device params: {:?}", char_device_cmd_params);

        let expected_params: Vec<String> = vec![
            "-chardev",
            "socket,id=charconsole0,path=/run/vc/vm/sandbox-id/console.sock,server,nowait",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(expected_params, char_device_cmd_params);
    }
}
