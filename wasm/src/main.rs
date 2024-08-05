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

use std::str::FromStr;

use clap::Parser;
use containerd_shim::asynchronous::monitor::monitor_notify_by_pid;
use futures::StreamExt;
use log::{debug, error, warn, LevelFilter};
use nix::{
    errno::Errno,
    libc,
    sys::{
        wait,
        wait::{WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use signal_hook_tokio::Signals;

use crate::sandbox::WasmSandboxer;

mod args;
mod sandbox;
mod utils;
mod version;
#[cfg(feature = "wasmedge")]
mod wasmedge;
#[cfg(feature = "wasmtime")]
mod wasmtime;

#[tokio::main]
async fn main() {
    let args = args::Args::parse();
    if args.version {
        version::print_version_info();
        return;
    }

    // Update args log level if it not presents args but in config.
    let log_level =
        LevelFilter::from_str(&args.log_level.unwrap_or_default()).unwrap_or(LevelFilter::Info);
    env_logger::Builder::from_default_env()
        .format_timestamp_micros()
        .filter_module("containerd_sandbox", log_level)
        .filter_module("wasm_sandboxer", log_level)
        .init();

    // TODO: Support recovery

    tokio::spawn(async move {
        let signals = Signals::new([libc::SIGPIPE, libc::SIGCHLD]).expect("new signal failed");
        handle_signals(signals).await;
    });

    let sandboxer = WasmSandboxer::default();
    containerd_sandbox::run(
        "kuasar-wasm-sandboxer-wasmedge",
        &args.listen,
        &args.dir,
        sandboxer,
    )
    .await
    .unwrap();
}

async fn handle_signals(signals: Signals) {
    let mut signals = signals.fuse();
    while let Some(sig) = signals.next().await {
        match sig {
            libc::SIGCHLD => loop {
                // Note: see comment at the counterpart in synchronous/mod.rs for details.
                match wait::waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(pid, status)) => {
                        debug!("child {} exit ({})", pid, status);
                        monitor_notify_by_pid(pid.as_raw(), status)
                            .await
                            .unwrap_or_else(|e| error!("failed to send exit event {}", e))
                    }
                    Ok(WaitStatus::Signaled(pid, sig, _)) => {
                        debug!("child {} terminated({})", pid, sig);
                        let exit_code = 128 + sig as i32;
                        monitor_notify_by_pid(pid.as_raw(), exit_code)
                            .await
                            .unwrap_or_else(|e| error!("failed to send signal event {}", e))
                    }
                    Err(Errno::ECHILD) => {
                        break;
                    }
                    Err(e) => {
                        warn!("error occurred in signal handler: {}", e);
                    }
                    _ => {}
                }
            },
            _ => {
                if let Ok(sig) = nix::sys::signal::Signal::try_from(sig) {
                    debug!("received {}", sig);
                } else {
                    warn!("received invalid signal {}", sig);
                }
            }
        }
    }
}
