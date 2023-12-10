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

use std::{io::IoSliceMut, ops::Deref, os::unix::io::RawFd, path::Path, sync::Arc};

use anyhow::anyhow;
use containerd_shim::{
    api::{ExecProcessRequest, Options},
    io::Stdio,
    io_error, other, other_error,
    util::IntoOption,
    Error,
};
use log::{debug, warn};
use nix::{
    cmsg_space,
    sys::{
        socket::{recvmsg, ControlMessageOwned, MsgFlags, UnixAddr},
        termios::tcgetattr,
    },
};
use oci_spec::runtime::{LinuxNamespaceType, Spec};
use runc::{
    io::{Io, NullIo, FIFO},
    options::GlobalOpts,
    Runc, Spawner,
};

pub const INIT_PID_FILE: &str = "init.pid";

pub struct ProcessIO {
    pub uri: Option<String>,
    pub io: Option<Arc<dyn Io>>,
    pub copy: bool,
}

pub fn create_io(
    id: &str,
    _io_uid: u32,
    _io_gid: u32,
    stdio: &Stdio,
) -> containerd_shim::Result<ProcessIO> {
    if stdio.is_null() {
        let nio = NullIo::new().map_err(io_error!(e, "new Null Io"))?;
        let pio = ProcessIO {
            uri: None,
            io: Some(Arc::new(nio)),
            copy: false,
        };
        return Ok(pio);
    }
    let stdout = stdio.stdout.as_str();
    let scheme_path = stdout.trim().split("://").collect::<Vec<&str>>();
    let scheme: &str;
    let uri: String;
    if scheme_path.len() <= 1 {
        // no scheme specified
        // default schema to fifo
        uri = format!("fifo://{}", stdout);
        scheme = "fifo"
    } else {
        uri = stdout.to_string();
        scheme = scheme_path[0];
    }

    let mut pio = ProcessIO {
        uri: Some(uri),
        io: None,
        copy: false,
    };

    if scheme == "fifo" {
        debug!(
            "create named pipe io for container {}, stdin: {}, stdout: {}, stderr: {}",
            id,
            stdio.stdin.as_str(),
            stdio.stdout.as_str(),
            stdio.stderr.as_str()
        );
        let io = FIFO {
            stdin: stdio.stdin.to_string().none_if(|x| x.is_empty()),
            stdout: stdio.stdout.to_string().none_if(|x| x.is_empty()),
            stderr: stdio.stderr.to_string().none_if(|x| x.is_empty()),
        };
        pio.io = Some(Arc::new(io));
        pio.copy = false;
    }
    Ok(pio)
}

#[derive(Default, Debug)]
pub struct ShimExecutor {}

pub fn get_spec_from_request(
    req: &ExecProcessRequest,
) -> containerd_shim::Result<oci_spec::runtime::Process> {
    if let Some(val) = req.spec.as_ref() {
        let mut p = serde_json::from_slice::<oci_spec::runtime::Process>(val.value.as_slice())?;
        p.set_terminal(Some(req.terminal));
        Ok(p)
    } else {
        Err(Error::InvalidArgument("no spec in request".to_string()))
    }
}

pub fn check_kill_error(emsg: String) -> Error {
    let emsg = emsg.to_lowercase();
    if emsg.contains("process already finished")
        || emsg.contains("container not running")
        || emsg.contains("no such process")
    {
        Error::NotFoundError("process already finished".to_string())
    } else if emsg.contains("does not exist") {
        Error::NotFoundError("no such container".to_string())
    } else {
        other!("unknown error after kill {}", emsg)
    }
}

const DEFAULT_RUNC_ROOT: &str = "/run/containerd/runc";
const DEFAULT_COMMAND: &str = "runc";

pub fn create_runc(
    runtime: &str,
    namespace: &str,
    bundle: impl AsRef<Path>,
    opts: &Options,
    spawner: Option<Arc<dyn Spawner + Send + Sync>>,
) -> containerd_shim::Result<Runc> {
    let runtime = if runtime.is_empty() {
        DEFAULT_COMMAND
    } else {
        runtime
    };
    let root = opts.root.as_str();
    let root = Path::new(if root.is_empty() {
        DEFAULT_RUNC_ROOT
    } else {
        root
    })
    .join(namespace);

    let log = bundle.as_ref().join("log.json");
    let mut gopts = GlobalOpts::default()
        .command(runtime)
        .root(root)
        .log(log)
        .log_json()
        .systemd_cgroup(opts.systemd_cgroup);
    if let Some(s) = spawner {
        gopts.custom_spawner(s);
    }
    gopts
        .build()
        .map_err(other_error!(e, "unable to create runc instance"))
}

#[derive(Default)]
pub(crate) struct CreateConfig {}

pub fn receive_socket(stream_fd: RawFd) -> containerd_shim::Result<RawFd> {
    let mut buf = [0u8; 4096];
    let mut iovec = [IoSliceMut::new(&mut buf)];
    let mut space = cmsg_space!([RawFd; 2]);
    let (path, fds) =
        match recvmsg::<UnixAddr>(stream_fd, &mut iovec, Some(&mut space), MsgFlags::empty()) {
            Ok(msg) => {
                let mut iter = msg.cmsgs();
                if let Some(ControlMessageOwned::ScmRights(fds)) = iter.next() {
                    (iovec[0].deref(), fds)
                } else {
                    return Err(other!("received message is empty"));
                }
            }
            Err(e) => {
                return Err(other!("failed to receive message: {}", e));
            }
        };
    if fds.is_empty() {
        return Err(other!("received message is empty"));
    }
    let path = String::from_utf8(Vec::from(path)).unwrap_or_else(|e| {
        warn!("failed to get path from array {}", e);
        "".to_string()
    });
    let path = path.trim_matches(char::from(0));
    debug!(
        "copy_console: console socket get path: {}, fd: {}",
        path, &fds[0]
    );
    tcgetattr(fds[0])?;
    Ok(fds[0])
}

pub fn has_shared_pid_namespace(spec: &Spec) -> bool {
    match spec.linux() {
        None => true,
        Some(linux) => match linux.namespaces() {
            None => true,
            Some(namespaces) => {
                for ns in namespaces {
                    if ns.typ() == LinuxNamespaceType::Pid && ns.path().is_none() {
                        return false;
                    }
                }
                true
            }
        },
    }
}

pub fn prepare_unix_socket(unix_socket: &str) -> Result<(), anyhow::Error> {
    let sock_path = Path::new(unix_socket);
    if sock_path.exists() {
        std::fs::remove_file(sock_path)?;
    }
    if let Some(sock_parent) = sock_path.parent() {
        std::fs::create_dir_all(sock_parent)
            .map_err(|e| anyhow!("failed to create {}, {}", sock_parent.display(), e))?;
    }
    Ok(())
}
