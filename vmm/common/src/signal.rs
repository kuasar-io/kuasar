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

use futures::StreamExt;
use nix::libc;
use signal_hook_tokio::Signals;

use crate::trace;

pub async fn handle_signals(log_level: &str, otlp_service_name: &str) {
    let mut signals = Signals::new([libc::SIGUSR1])
        .expect("new signal failed")
        .fuse();

    while let Some(sig) = signals.next().await {
        if sig == libc::SIGUSR1 {
            trace::set_enabled(!trace::is_enabled());
            let _ = trace::setup_tracing(log_level, otlp_service_name);
        }
    }
}
