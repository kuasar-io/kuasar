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
pub mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub fn print_version_info() {
    if let Some(v) = built_info::GIT_VERSION {
        match built_info::GIT_DIRTY {
            Some(true) => println!("Version: {}-dirty", v),
            _ => println!("Version: {}", v),
        }
    }
    println!("Build Time: {}", built_info::BUILT_TIME_UTC)
}
