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

use std::{io, path::Path};

use containerd_shim::{
    asynchronous::{
        monitor::{monitor_subscribe, monitor_unsubscribe, Subscription},
        util::read_file_to_str,
    },
    error::{Error, Result},
    monitor::{ExitEvent, Subject, Topic},
    other, other_error,
};
use log::{debug, error};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use vmm_common::{storage::Storage, Io, IO_FILE_PREFIX, STORAGE_FILE_PREFIX};

pub async fn wait_pid(pid: i32, s: Subscription) -> i32 {
    let mut s = s;
    loop {
        if let Some(ExitEvent {
            subject: Subject::Pid(epid),
            exit_code: code,
        }) = s.rx.recv().await
        {
            if pid == epid {
                monitor_unsubscribe(s.id).await.unwrap_or_default();
                return code;
            }
        }
    }
}

#[cfg_attr(feature = "youki", allow(dead_code))]
pub(crate) async fn read_std<T>(std: Option<T>) -> String
where
    T: AsyncRead + Unpin,
{
    let mut std = std;
    if let Some(mut std) = std.take() {
        let mut out = String::new();
        if let Err(e) = std.read_to_string(&mut out).await {
            error!("failed to read process stdio: {}", e);
        }
        return out;
    }
    "".to_string()
}

async fn read_std_bytes<T>(std: Option<T>) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin,
{
    let mut std = std;
    if let Some(mut std) = std.take() {
        let mut out = Vec::new();
        std.read_to_end(&mut out).await?;
        return Ok(out);
    }
    Ok(Vec::new())
}

pub async fn spawn_and_wait(
    mut cmd: std::process::Command,
    stdin: &[u8],
) -> Result<(String, String, i32)> {
    let subscription = monitor_subscribe(Topic::Pid).await?;
    let sid = subscription.id;
    let mut child = cmd.spawn().map_err(|e| Error::IoError {
        context: "spawn failed".to_string(),
        err: e,
    })?;

    let pid = child.id();

    if !stdin.is_empty() {
        if let Some(child_stdin) = child.stdin.take() {
            let mut async_stdin =
                ChildStdin::from_std(child_stdin).map_err(|e| Error::IoError {
                    context: "failed to convert stdin".to_string(),
                    err: e,
                })?;
            async_stdin
                .write_all(stdin)
                .await
                .map_err(|e| Error::IoError {
                    context: "failed to write stdin".to_string(),
                    err: e,
                })?;
        } else {
            return Err(other!(
                "stdin pipe is missing, please ensure stdin(Stdio::piped()) is set"
            ));
        }
    }

    let stdout_stream = child
        .stdout
        .take()
        .and_then(|s| ChildStdout::from_std(s).ok());
    let stderr_stream = child
        .stderr
        .take()
        .and_then(|s| ChildStderr::from_std(s).ok());

    let (stdout, stderr, exit_code) = tokio::join!(
        read_std_bytes(stdout_stream),
        read_std_bytes(stderr_stream),
        async move {
            #[cfg(test)]
            {
                let pid = pid;
                tokio::spawn(async move {
                    let wait_result = tokio::task::spawn_blocking(move || {
                        use std::os::unix::process::ExitStatusExt;

                        let mut child = child;
                        child.wait().map(|status| {
                            status
                                .code()
                                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
                        })
                    })
                    .await;
                    match wait_result {
                        Ok(Ok(code)) => {
                            let _ = containerd_shim::asynchronous::monitor::monitor_notify_by_pid(
                                pid as i32, code,
                            )
                            .await;
                        }
                        Ok(Err(e)) => {
                            error!("test reaper failed to wait child {}: {}", pid, e);
                        }
                        Err(e) => {
                            error!("test reaper task failed for child {}: {}", pid, e);
                        }
                    }
                });
            }
            wait_pid(pid as i32, subscription).await
        }
    );

    let stdout = String::from_utf8(stdout.map_err(|e| Error::IoError {
        context: "failed to read stdout".to_string(),
        err: e,
    })?)
    .map_err(other_error!(e, "failed to convert stdout"))?;
    let stderr = String::from_utf8_lossy(&stderr.map_err(|e| Error::IoError {
        context: "failed to read stderr".to_string(),
        err: e,
    })?)
    .into_owned();

    Ok((stdout, stderr, exit_code))
}

pub async fn read_storages(bundle: impl AsRef<Path>, id: &str) -> Result<Vec<Storage>> {
    let storage_file_name = format!("{}-{}", STORAGE_FILE_PREFIX, id);
    let path = bundle.as_ref().join(&storage_file_name);
    let content = read_file_to_str(&path).await?;
    serde_json::from_str::<Vec<Storage>>(content.as_str()).map_err(other_error!(e, "read storage"))
}

pub async fn read_io(
    bundle: impl AsRef<Path>,
    container_id: &str,
    exec_id: Option<&str>,
) -> Result<Io> {
    let io_file_name = if let Some(eid) = exec_id {
        format!("{}-{}-{}", IO_FILE_PREFIX, container_id, eid)
    } else {
        format!("{}-{}", IO_FILE_PREFIX, container_id)
    };
    let path = bundle.as_ref().join(&io_file_name);
    let content = read_file_to_str(&path).await?;
    let io = serde_json::from_str::<Io>(content.as_str()).map_err(other_error!(e, "read io"))?;
    if exec_id.is_some() {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::IoError {
                    context: "remove exec io file".to_string(),
                    err: e,
                })
            }
        }
    }
    Ok(io)
}

#[cfg(test)]
mod tests {
    use containerd_sandbox::data::Io;
    use containerd_shim::util::write_str_to_file;
    use temp_dir::TempDir;

    use super::read_io;

    #[tokio::test]
    async fn test_read_io_cleanup_behavior() {
        struct TestCase {
            name: &'static str,
            exec_id: Option<&'static str>,
            terminal: bool,
            should_remove: bool,
        }

        let cases = [
            TestCase {
                name: "exec io is removed after read",
                exec_id: Some("test-exec"),
                terminal: false,
                should_remove: true,
            },
            TestCase {
                name: "init io is kept after read",
                exec_id: None,
                terminal: true,
                should_remove: false,
            },
        ];

        for case in cases {
            let tmp_dir = TempDir::new().unwrap();
            let bundle = tmp_dir.path();
            let io_file_name = match case.exec_id {
                Some(exec_id) => format!("io-test-container-{}", exec_id),
                None => "io-test-container".to_string(),
            };
            let io_path = bundle.join(io_file_name);
            let io = Io {
                stdin: "stdin".to_string(),
                stdout: "stdout".to_string(),
                stderr: "stderr".to_string(),
                terminal: case.terminal,
            };
            let io_str = serde_json::to_string(&io).unwrap();
            write_str_to_file(&io_path, &io_str).await.unwrap();

            let read_back = read_io(bundle, "test-container", case.exec_id)
                .await
                .unwrap();

            assert_eq!(read_back.stdin, "stdin", "case: {}", case.name);
            assert_eq!(io_path.exists(), !case.should_remove, "case: {}", case.name);
        }
    }
}
