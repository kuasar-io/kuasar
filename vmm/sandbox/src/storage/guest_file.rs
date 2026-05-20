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
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::anyhow;
use containerd_sandbox::error::{Error, Result};
use ttrpc::context::with_timeout;
use vmm_common::api::{sandbox::ExecVMProcessRequest, sandbox_ttrpc::SandboxServiceClient};

pub(crate) const DRIVER_GUEST_FILE: &str = "guest-file";

const GUEST_EXEC_TIMEOUT_SECS: u64 = 10;

/// Pre-scanned directory contents for size-threshold checking and guest-file injection.
/// Created by `scan_dir`; passed to `GuestFileInjector::inject_from_scan` to avoid a
/// second directory traversal when the directory is small enough for file injection.
#[derive(Debug)]
pub(crate) struct ScannedDir {
    pub(crate) file_count: usize,
    pub(crate) total_bytes: u64,
    dirs: Vec<(PathBuf, PathBuf)>,       // (host_path, rel_path)
    files: Vec<(PathBuf, PathBuf, u32)>, // (host_path, rel_path, mode)
    symlinks: Vec<PathBuf>,
}

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

    /// Inject pre-scanned directory contents into the guest, reusing the entries
    /// collected by `scan_dir` to avoid re-traversing the host directory.
    pub(crate) async fn inject_from_scan(&self, dest_dir: &str, scan: &ScannedDir) -> Result<()> {
        if let Some(symlink) = scan.symlinks.first() {
            return Err(Error::InvalidArgument(format!(
                "symlink {} is not supported by virtio-blk guest-file injection",
                symlink.display()
            )));
        }
        self.ensure_dir(dest_dir).await?;
        for (_, rel) in &scan.dirs {
            let guest_path = join_guest_path(dest_dir, rel.as_path())?;
            self.ensure_dir(&guest_path).await?;
        }
        for (host_path, rel, mode) in &scan.files {
            let guest_path = join_guest_path(dest_dir, rel.as_path())?;
            let content = tokio::fs::read(host_path)
                .await
                .map_err(|e| anyhow!("read {}: {}", host_path.display(), e))?;
            self.push_file(&guest_path, content, *mode).await?;
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

/// Scan a directory tree once, collecting both the size/count stats needed for the
/// small-vs-large threshold decision and the entry list needed for injection.
/// Passing the result to `GuestFileInjector::inject_from_scan` avoids a second traversal.
pub(crate) async fn scan_dir(dir: &str) -> Result<ScannedDir> {
    let root = PathBuf::from(dir);
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut files: Vec<(PathBuf, PathBuf, u32)> = Vec::new();
    let mut symlinks: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(current) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&current)
            .await
            .map_err(|e| anyhow!("read_dir {}: {}", current.display(), e))?;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| anyhow!("read entry in {}: {}", current.display(), e))?
        {
            let path = entry.path();
            let rel = path
                .strip_prefix(&root)
                .map_err(|e| anyhow!("strip prefix {}: {}", path.display(), e))?
                .to_path_buf();
            let ft = entry.file_type().await.map_err(|e| anyhow!("{}", e))?;
            if ft.is_dir() {
                dirs.push((path.clone(), rel));
                stack.push(path);
            } else if ft.is_file() {
                let meta = entry.metadata().await.map_err(|e| anyhow!("{}", e))?;
                let mode = meta.permissions().mode() & 0o777;
                file_count += 1;
                total_bytes += meta.len();
                files.push((path, rel, mode));
            } else if ft.is_symlink() {
                // Counted toward threshold; rejected at inject time.
                file_count += 1;
                symlinks.push(path);
            } else {
                return Err(Error::InvalidArgument(format!(
                    "special file {} is not supported by virtio-blk bind snapshot",
                    path.display()
                )));
            }
        }
    }
    Ok(ScannedDir {
        file_count,
        total_bytes,
        dirs,
        files,
        symlinks,
    })
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
