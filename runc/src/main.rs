use containerd_shim::asynchronous::monitor::monitor_notify_by_pid;
use futures::StreamExt;
use log::{debug, error, warn};
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
use crate::sandbox::RuncSandboxer;

mod sandbox;
mod runc;
mod common;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder().format_timestamp_micros().init();
    tokio::spawn(async move {
        let signals = Signals::new([libc::SIGTERM, libc::SIGINT, libc::SIGPIPE, libc::SIGCHLD])
            .expect("new signal failed");
        handle_signals(signals).await;
    });

    let sandboxer = RuncSandboxer::default();
    containerd_sandbox::run("runc-sandboxer", sandboxer)
        .await
        .unwrap();
    Ok(())
}

async fn handle_signals(signals: Signals) {
    let mut signals = signals.fuse();
    while let Some(sig) = signals.next().await {
        match sig {
            libc::SIGTERM | libc::SIGINT => {
                debug!("received {}", sig);
            }
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
