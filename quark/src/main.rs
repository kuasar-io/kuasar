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

use std::sync::Arc;

use clap::Parser;

use crate::sandbox::QuarkSandboxer;

mod args;
mod mount;
mod sandbox;
mod utils;

#[tokio::main]
async fn main() {
    let args = args::Args::parse();
    env_logger::Builder::from_default_env()
        .format_timestamp_micros()
        .init();
    let sandboxer = QuarkSandboxer {
        sandboxes: Arc::new(Default::default()),
    };
    crate::utils::start_watchdog();
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        log::error!("failed to send ready notify: {}", e);
    }
    containerd_sandbox::run("quark-sandboxer", &args.listen, &args.dir, sandboxer)
        .await
        .unwrap();
}
