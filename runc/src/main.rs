use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::process::{exit, id};

use anyhow::anyhow;
use byteorder::WriteBytesExt;
use containerd_shim::asynchronous::monitor::monitor_notify_by_pid;
use futures::StreamExt;
use log::{debug, error, warn};
use nix::{errno::Errno, libc, NixPath, sys::{
    wait,
    wait::{WaitPidFlag, WaitStatus},
}, unistd::Pid};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::sched::{CloneFlags, setns, unshare};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, sigaction, SIGCHLD};
use nix::sys::stat::Mode;
use nix::unistd::{close, fork, ForkResult, pause, pipe, read, write};
use prctl::PrctlMM;
use signal_hook_tokio::Signals;

use crate::sandbox::{RuncSandboxer, SandboxParent};

mod sandbox;
mod runc;
mod common;

fn main() {
    env_logger::builder().format_timestamp_micros().init();
    let sandbox_parent = fork_sandbox_parent().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async move {
        start_sandboxer(sandbox_parent).await.unwrap();
    });
}

// Call it instantly after enter the main function,
// it will fork a process and make it as the parent of all the sandbox processes.
// any time we want to fork a sandbox process, we will send a message to this parent process
// and this parent will fork a process for sandbox and return the pid.
fn fork_sandbox_parent() -> Result<SandboxParent, anyhow::Error> {
    let (reqr, reqw) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;
    let (respr, respw) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;

    match unsafe { fork().map_err(|e| anyhow!("failed to fork sandbox parent {}", e))? } {
        ForkResult::Parent { child } => {
            debug!("forked process {} for the sandbox parent", child);
            close(reqr).unwrap_or_default();
            close(respw).unwrap_or_default();
        }
        ForkResult::Child => {
            close(reqw).unwrap_or_default();
            close(respr).unwrap_or_default();
            let comm = format!("[sandbox-parent]");
            let comm_cstr = CString::new(comm).unwrap();
            let addr = comm_cstr.as_ptr();
            set_process_comm(addr as u64, comm_cstr.as_bytes_with_nul().len() as u64);
            let sig_action = SigAction::new(
                SigHandler::Handler(sandbox_parent_handle_signals),
                SaFlags::empty(),
                SigSet::empty()
            );
            unsafe {sigaction(SIGCHLD, &sig_action).unwrap();}
            loop {
                let buffer = read_count(reqr, 512).unwrap();
                let id = String::from_utf8_lossy(&buffer[0..64]).to_string();
                let mut zero_index = 64;
                for i in 64..512 {
                    if buffer[i] == 0 {
                        zero_index = i;
                        break;
                    }
                }
                let netns = String::from_utf8_lossy(&buffer[64..zero_index]).to_string();
                let sandbox_pid = fork_sandbox(&id, &netns).unwrap();
                write_all(respw, sandbox_pid.to_le_bytes().as_slice()).unwrap();
            }
        }
    }
    fcntl(reqw, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap_or_default();
    fcntl(respr, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).unwrap_or_default();
    Ok(SandboxParent::new(reqw, respr))
}

pub fn read_count(fd: RawFd, count: usize) -> Result<Vec<u8>, anyhow::Error> {
    let mut buf = vec![0u8; count];
    let mut idx = 0;
    loop {
        let l = match read(fd, &mut buf[idx..]) {
            Ok(l) => l,
            Err(e) => {
                if e == Errno::EINTR {
                    continue;
                } else {
                    return Err(anyhow!("failed to read from pipe {}", e));
                }
            }
        };
        idx += l;
        if idx == count || l == 0 {
            return Ok(buf);
        }
    }
}

pub fn write_all(fd: RawFd, buf: &[u8]) -> Result<(), anyhow::Error> {
    let mut idx = 0;
    let count = buf.len();
    loop {
        let l = match write(fd, &buf[idx..]) {
            Ok(l) => l,
            Err(e) => {
                if e == Errno::EINTR {
                    continue;
                } else {
                    return Err(anyhow!("failed to write to pipe {}", e));
                }
            }
        };
        idx += l;
        if idx == count {
            return Ok(());
        }
    }
}

fn fork_sandbox(id: &str, netns: &str) -> Result<i32, anyhow::Error> {
    match unsafe { fork().map_err(|e| anyhow!("failed to fork sandbox {}", e))? } {
        ForkResult::Parent { child } => {
            debug!("forked process {} for the sandbox {}", child, id);
            return Ok(child.as_raw());
        }
        ForkResult::Child => {
            let comm = format!("[sandbox-{}]", id);
            let comm_cstr = CString::new(comm).unwrap();
            let addr = comm_cstr.as_ptr();
            set_process_comm(addr as u64, comm_cstr.as_bytes_with_nul().len() as u64);
            if !netns.is_empty() {
                let netns_fd =
                    nix::fcntl::open(&*netns, OFlag::O_CLOEXEC, Mode::empty()).unwrap();
                setns(netns_fd, CloneFlags::CLONE_NEWNET).unwrap();
            }
            unshare(CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID).unwrap();
            loop {
                pause();
            }
        }
    }
}

fn set_process_comm(addr: u64, len: u64) {
    if let Err(_)  = prctl::set_mm(PrctlMM::PR_SET_MM_ARG_START, addr) {
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_END, addr + len).unwrap();
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_START, addr).unwrap()
    } else {
        prctl::set_mm(PrctlMM::PR_SET_MM_ARG_END,  addr + len).unwrap();
    }
}

async fn start_sandboxer(sandbox_parent: SandboxParent) -> anyhow::Result<()> {
    tokio::spawn(async move {
        let signals = Signals::new([libc::SIGTERM, libc::SIGINT, libc::SIGPIPE, libc::SIGCHLD])
            .expect("new signal failed");
        handle_signals(signals).await;
    });
    prctl::set_child_subreaper(true).unwrap();

    let sandboxer = RuncSandboxer::new(sandbox_parent);
    containerd_sandbox::run("runc-sandboxer", sandboxer)
        .await
        .unwrap();
    Ok(())
}

extern "C" fn sandbox_parent_handle_signals(_: libc::c_int) {
    loop {
        match wait::waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                debug!("child {} exit ({})", pid, status);
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                debug!("child {} terminated({})", pid, sig);
            }
            Err(Errno::ECHILD) | Ok(WaitStatus::StillAlive) => {
                break;
            }
            Err(e) => {
                warn!("error occurred in signal handler: {}", e);
            }
            _ => {}
        }
    }
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
                    Err(Errno::ECHILD) | Ok(WaitStatus::StillAlive) => {
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
