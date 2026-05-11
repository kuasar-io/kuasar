/*
Copyright 2026 The Kuasar Authors.

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

use std::{
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
    time::Duration,
};

use anyhow::anyhow;
use containerd_sandbox::error::{Error, Result};
use ttrpc::context::with_timeout;
use vmm_common::api::{sandbox::ExecVMProcessRequest, sandbox_ttrpc::SandboxServiceClient};

pub(crate) const DRIVER_GUEST_FILE: &str = "guest-file";

const GUEST_EXEC_TIMEOUT_SECS: u64 = 10;

pub(crate) struct GuestFileInjector<'a> {
    client: &'a SandboxServiceClient,
}

impl<'a> GuestFileInjector<'a> {
    pub(crate) fn new(client: &'a SandboxServiceClient) -> Self {
        Self { client }
    }

    pub(crate) async fn ensure_dir(&self, dest_dir: &str) -> Result<()> {
        validate_guest_path(dest_dir)?;
        self.exec(&format!("mkdir -p {}", shell_quote(dest_dir)))
            .await
    }

    pub(crate) async fn push_file(
        &self,
        dest_path: &str,
        content: Vec<u8>,
        mode: u32,
    ) -> Result<()> {
        validate_guest_path(dest_path)?;
        let parent = Path::new(dest_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/tmp".to_string());
        validate_guest_path(&parent)?;

        let qparent = shell_quote(&parent);
        let qdest = shell_quote(dest_path);
        let mut req = ExecVMProcessRequest::new();
        req.command = format!(
            "mkdir -p {} && cat > {} && chmod {:o} {}",
            qparent, qdest, mode, qdest
        );
        req.stdin = content;
        self.exec_request(req)
            .await
            .map_err(|e| anyhow!("push file to guest at {}: {}", dest_path, e).into())
    }

    pub(crate) async fn inject_dir(&self, src_dir: &str, dest_dir: &str) -> Result<()> {
        self.ensure_dir(dest_dir).await?;

        let mut stack = vec![src_dir.to_string()];
        while let Some(dir) = stack.pop() {
            let mut entries = tokio::fs::read_dir(&dir)
                .await
                .map_err(|e| anyhow!("read dir {}: {}", dir, e))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| anyhow!("read entry in {}: {}", dir, e))?
            {
                let path = entry.path();
                let rel = path
                    .strip_prefix(src_dir)
                    .map_err(|e| anyhow!("strip prefix {}: {}", path.display(), e))?;
                let guest_path = join_guest_path(dest_dir, rel)?;
                let ft = entry
                    .file_type()
                    .await
                    .map_err(|e| anyhow!("file_type {}: {}", path.display(), e))?;
                if ft.is_dir() {
                    self.ensure_dir(&guest_path).await?;
                    stack.push(path.to_string_lossy().to_string());
                } else if ft.is_file() {
                    let meta = entry
                        .metadata()
                        .await
                        .map_err(|e| anyhow!("metadata {}: {}", path.display(), e))?;
                    let mode = meta.permissions().mode() & 0o777;
                    let content = tokio::fs::read(&path)
                        .await
                        .map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
                    self.push_file(&guest_path, content, mode).await?;
                } else if ft.is_symlink() {
                    return Err(Error::InvalidArgument(format!(
                        "symlink {} is not supported by virtio-blk guest-file injection",
                        path.display()
                    )));
                } else {
                    return Err(Error::InvalidArgument(format!(
                        "special file {} is not supported by virtio-blk guest-file injection",
                        path.display()
                    )));
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn exec(&self, command: &str) -> Result<()> {
        let mut req = ExecVMProcessRequest::new();
        req.command = command.to_string();
        req.stdin = vec![];
        self.exec_request(req)
            .await
            .map_err(|e| anyhow!("exec in guest '{}': {}", command, e).into())
    }

    async fn exec_request(&self, req: ExecVMProcessRequest) -> Result<()> {
        let timeout_ns = Duration::from_secs(GUEST_EXEC_TIMEOUT_SECS).as_nanos() as i64;
        self.client
            .exec_vm_process(with_timeout(timeout_ns), &req)
            .await
            .map_err(|e| anyhow!("exec_vm_process: {}", e))?;
        Ok(())
    }
}

// Count entries and total regular-file bytes in a directory tree using iterative DFS.
//
// Symlinks are counted as entries (size 0) so that a directory containing symlinks
// is measured correctly for the small-dir threshold.  The actual guest-file injection
// path (inject_dir) rejects symlinks explicitly; the block-image path (rsync -aHAX)
// handles them transparently.  Special files (device nodes, FIFOs, sockets) are still
// rejected here because no delivery path supports them.
pub(crate) async fn count_dir_contents(dir: &str) -> Result<(usize, u64)> {
    let mut count = 0usize;
    let mut total = 0u64;
    let mut stack = vec![dir.to_string()];
    while let Some(d) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&d)
            .await
            .map_err(|e| anyhow!("read_dir {}: {}", d, e))?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| anyhow!("{}", e))? {
            let ft = entry.file_type().await.map_err(|e| anyhow!("{}", e))?;
            if ft.is_dir() {
                stack.push(entry.path().to_string_lossy().to_string());
            } else if ft.is_file() {
                let meta = entry.metadata().await.map_err(|e| anyhow!("{}", e))?;
                count += 1;
                total += meta.len();
            } else if ft.is_symlink() {
                // Counted as one entry with zero byte contribution to the size total.
                // Delivery-path rejection happens later (inject_dir for guest-file,
                // rsync for block images).
                count += 1;
            } else {
                return Err(Error::InvalidArgument(format!(
                    "special file {} is not supported by virtio-blk bind snapshot",
                    entry.path().display()
                )));
            }
        }
    }
    Ok((count, total))
}

// Wrap a path in single quotes and escape any embedded single quotes.
// This prevents shell injection regardless of the path contents.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// Validate that a path was generated internally and contains only safe characters.
// Used as an additional sanity check for paths built from known-safe constants.
pub(crate) fn validate_guest_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::InvalidArgument("guest path is empty".to_string()));
    }
    validate_raw_components(path, true)?;

    for component in Path::new(path).components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                validate_guest_component(value.to_str().ok_or_else(|| {
                    Error::InvalidArgument(format!("guest path is not valid UTF-8: {}", path))
                })?)?
            }
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "guest path contains unsafe component: {}",
                    path
                )))
            }
        }
    }
    Ok(())
}

pub(crate) fn join_guest_path(base: &str, rel: &Path) -> Result<String> {
    validate_guest_path(base)?;
    validate_relative_guest_path(rel)?;

    let mut path = base.trim_end_matches('/').to_string();
    for component in rel.components() {
        if let Component::Normal(value) = component {
            path.push('/');
            path.push_str(value.to_str().ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "guest relative path is not valid UTF-8: {}",
                    rel.display()
                ))
            })?);
        }
    }
    Ok(path)
}

pub(crate) fn join_guest_component(base: &str, component: &str) -> Result<String> {
    validate_guest_path(base)?;
    if component.contains('/') || component.contains('\\') {
        return Err(Error::InvalidArgument(format!(
            "guest path component contains path separator: {}",
            component
        )));
    }
    validate_guest_component(component)?;
    Ok(format!("{}/{}", base.trim_end_matches('/'), component))
}

fn validate_relative_guest_path(path: &Path) -> Result<()> {
    validate_raw_components(
        path.to_str().ok_or_else(|| {
            Error::InvalidArgument(format!(
                "guest relative path is not valid UTF-8: {}",
                path.display()
            ))
        })?,
        false,
    )?;

    for component in path.components() {
        match component {
            Component::Normal(value) => {
                validate_guest_component(value.to_str().ok_or_else(|| {
                    Error::InvalidArgument(format!(
                        "guest relative path is not valid UTF-8: {}",
                        path.display()
                    ))
                })?)?
            }
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "guest relative path contains unsafe component: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

fn validate_raw_components(path: &str, allow_absolute: bool) -> Result<()> {
    for (index, component) in path.split('/').enumerate() {
        if component.is_empty() {
            if allow_absolute && index == 0 && path.starts_with('/') {
                continue;
            }
            return Err(Error::InvalidArgument(format!(
                "guest path contains empty component: {}",
                path
            )));
        }
        if component == "." || component == ".." {
            return Err(Error::InvalidArgument(format!(
                "guest path contains unsafe component: {}",
                path
            )));
        }
    }
    Ok(())
}

fn validate_guest_component(component: &str) -> Result<()> {
    if component.is_empty() || component == "." || component == ".." {
        return Err(Error::InvalidArgument(format!(
            "guest path contains unsafe component: {}",
            component
        )));
    }
    if !component
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
    {
        return Err(Error::InvalidArgument(format!(
            "guest path component contains unsafe characters: {}",
            component
        )));
    }
    Ok(())
}
