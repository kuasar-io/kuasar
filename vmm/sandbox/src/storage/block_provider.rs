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

use anyhow::anyhow;
use async_trait::async_trait;
use containerd_sandbox::error::Result;
use log::warn;
use nix::libc::MNT_DETACH;
use vmm_common::{
    mount::{unmount, MNT_NOFOLLOW},
    storage::Storage,
};

const MIN_EXT4_INODES: u64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockFs {
    Ext4,
    Xfs,
}

impl BlockFs {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BlockPrepareRequest {
    pub(crate) src_dir: String,
    pub(crate) img_path: String,
    pub(crate) fstype: BlockFs,
    pub(crate) fallback_size_mb: u64,
    pub(crate) overhead_percent: u32,
    pub(crate) source_identity: Option<String>,
    /// Whether the image will be attached to the guest read-only.
    pub(crate) readonly: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct BlockArtifact {
    pub(crate) path: String,
    pub(crate) fstype: String,
    #[allow(dead_code)]
    pub(crate) readonly: bool,
    pub(crate) owned_by_runtime: bool,
    pub(crate) cleanup_path: Option<String>,
    pub(crate) source_identity: Option<String>,
}

impl BlockArtifact {
    pub(crate) fn from_storage(storage: &Storage) -> Option<Self> {
        if !storage.owned_by_runtime {
            return None;
        }
        let cleanup_path = storage.cleanup_path.clone()?;
        Some(Self {
            path: cleanup_path.clone(),
            fstype: storage.fstype.clone(),
            readonly: storage.options.contains(&"ro".to_string()),
            owned_by_runtime: storage.owned_by_runtime,
            cleanup_path: Some(cleanup_path),
            source_identity: storage.source_identity.clone(),
        })
    }
}

#[async_trait]
pub(crate) trait BlockProvider {
    async fn prepare(&self, req: BlockPrepareRequest) -> Result<BlockArtifact>;
    async fn release(&self, artifact: &BlockArtifact) -> Result<()>;
}

#[derive(Default)]
pub(crate) struct LocalBlockProvider;

#[async_trait]
impl BlockProvider for LocalBlockProvider {
    async fn prepare(&self, req: BlockPrepareRequest) -> Result<BlockArtifact> {
        let stats = estimate_dir_tree_stats(&req.src_dir).await.ok();
        let base = stats
            .as_ref()
            .map(|s| s.size_mb)
            .unwrap_or(req.fallback_size_mb);
        let inode_count = stats
            .as_ref()
            .map(|s| s.ext4_inode_count(req.overhead_percent))
            .unwrap_or(MIN_EXT4_INODES);
        let size_mb = apply_overhead(base, req.overhead_percent) + req.fallback_size_mb;

        let prepare_result = async {
            match req.fstype {
                BlockFs::Ext4 => {
                    create_ext4_image_with_inodes(&req.img_path, size_mb, inode_count).await?;
                    copy_dir_to_ext4(&req.src_dir, &req.img_path).await
                }
                BlockFs::Xfs => {
                    create_xfs_image(&req.img_path, size_mb).await?;
                    copy_dir_to_xfs(&req.src_dir, &req.img_path).await
                }
            }
        }
        .await;

        if let Err(e) = prepare_result {
            let _ = tokio::fs::remove_file(&req.img_path).await;
            return Err(e);
        }

        Ok(BlockArtifact {
            path: req.img_path.clone(),
            fstype: req.fstype.as_str().to_string(),
            readonly: req.readonly,
            owned_by_runtime: true,
            cleanup_path: Some(req.img_path),
            source_identity: req.source_identity,
        })
    }

    async fn release(&self, artifact: &BlockArtifact) -> Result<()> {
        if !artifact.owned_by_runtime {
            return Ok(());
        }
        let cleanup_path = artifact.cleanup_path.as_ref().ok_or_else(|| {
            anyhow!(
                "runtime-owned block artifact {} is missing cleanup_path",
                artifact.path
            )
        })?;
        if let Err(e) = tokio::fs::remove_file(cleanup_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("failed to remove block artifact {}: {}", cleanup_path, e);
            }
        }
        Ok(())
    }
}

fn apply_overhead(base: u64, overhead_percent: u32) -> u64 {
    base * (100 + overhead_percent as u64) / 100
}

#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub(crate) struct DirTreeStats {
    pub(crate) size_mb: u64,
    pub(crate) entries: u64,
}

impl DirTreeStats {
    fn ext4_inode_count(&self, overhead_percent: u32) -> u64 {
        apply_overhead(self.entries.max(1), overhead_percent).max(MIN_EXT4_INODES)
    }
}

pub(crate) async fn estimate_dir_size_mb(dir: &str) -> Result<u64> {
    let output = tokio::process::Command::new("du")
        .args(["-sm", dir])
        .output()
        .await
        .map_err(|e| anyhow!("du -sm {}: {}", dir, e))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let size_mb = stdout
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(64);
    Ok(size_mb)
}

pub(crate) async fn estimate_dir_tree_stats(dir: &str) -> Result<DirTreeStats> {
    let size_mb = estimate_dir_size_mb(dir).await?;
    let mut entries = 1u64; // root directory consumes an inode too.
    let mut stack = vec![dir.to_string()];

    while let Some(current) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&current)
            .await
            .map_err(|e| anyhow!("read_dir {}: {}", current, e))?;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| anyhow!("read entry in {}: {}", current, e))?
        {
            entries += 1;
            let path = entry.path();
            let meta = tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|e| anyhow!("metadata {}: {}", path.display(), e))?;
            if meta.is_dir() {
                stack.push(path.to_string_lossy().to_string());
            }
        }
    }

    Ok(DirTreeStats { size_mb, entries })
}

pub(crate) async fn create_ext4_image_with_inodes(
    path: &str,
    size_mb: u64,
    inode_count: u64,
) -> Result<()> {
    let file = tokio::fs::File::create(path)
        .await
        .map_err(|e| anyhow!("create ext4 image {}: {}", path, e))?;
    file.set_len(size_mb * 1024 * 1024)
        .await
        .map_err(|e| anyhow!("set ext4 image size: {}", e))?;
    drop(file);

    let inode_count = inode_count.max(MIN_EXT4_INODES).to_string();
    let status = tokio::process::Command::new("mkfs.ext4")
        .arg("-F")
        .arg("-O")
        .arg("^has_journal")
        .arg("-E")
        .arg("lazy_itable_init=0,lazy_journal_init=0")
        .arg("-N")
        .arg(inode_count)
        .arg(path)
        .status()
        .await
        .map_err(|e| anyhow!("mkfs.ext4 {}: {}", path, e))?;
    if !status.success() {
        return Err(anyhow!("mkfs.ext4 failed for {}: {:?}", path, status).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use temp_dir::TempDir;

    use super::*;

    #[test]
    fn ext4_inode_count_has_minimum_and_overhead() {
        let small = DirTreeStats {
            size_mb: 1,
            entries: 2,
        };
        assert_eq!(small.ext4_inode_count(20), MIN_EXT4_INODES);

        let large = DirTreeStats {
            size_mb: 1,
            entries: 10_000,
        };
        assert_eq!(large.ext4_inode_count(20), 12_000);
    }

    #[tokio::test]
    async fn estimate_dir_tree_stats_counts_nested_entries() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(dir.path().join("file-a"), b"a")
            .await
            .unwrap();
        tokio::fs::write(nested.join("file-b"), b"b").await.unwrap();
        symlink("file-a", dir.path().join("link-a")).unwrap();

        let stats = estimate_dir_tree_stats(dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(stats.entries, 5);
        assert!(stats.size_mb >= 1);
    }
}

pub(crate) async fn copy_dir_to_ext4(src_dir: &str, img_path: &str) -> Result<()> {
    copy_dir_to_loop_image(src_dir, img_path, "ext4").await
}

pub(crate) async fn create_xfs_image(path: &str, size_mb: u64) -> Result<()> {
    let file = tokio::fs::File::create(path)
        .await
        .map_err(|e| anyhow!("create xfs image {}: {}", path, e))?;
    file.set_len(size_mb * 1024 * 1024)
        .await
        .map_err(|e| anyhow!("set xfs image size: {}", e))?;
    drop(file);

    let status = tokio::process::Command::new("mkfs.xfs")
        .args(["-f", "-m", "reflink=1", path])
        .status()
        .await
        .map_err(|e| anyhow!("mkfs.xfs {}: {}", path, e))?;
    if !status.success() {
        return Err(anyhow!("mkfs.xfs failed for {}: {:?}", path, status).into());
    }
    Ok(())
}

pub(crate) async fn copy_dir_to_xfs(src_dir: &str, img_path: &str) -> Result<()> {
    copy_dir_to_loop_image(src_dir, img_path, "xfs").await
}

async fn copy_dir_to_loop_image(src_dir: &str, img_path: &str, fstype: &str) -> Result<()> {
    let mnt_dir = format!("{}.mnt", img_path);
    tokio::fs::create_dir_all(&mnt_dir)
        .await
        .map_err(|e| anyhow!("create mnt dir {}: {}", mnt_dir, e))?;

    let mount_status = tokio::process::Command::new("mount")
        .args(["-o", "loop", img_path, &mnt_dir])
        .status()
        .await
        .map_err(|e| anyhow!("mount loop {}: {}", img_path, e))?;
    if !mount_status.success() {
        let _ = tokio::fs::remove_dir(&mnt_dir).await;
        return Err(anyhow!("mount loop failed for {}: {:?}", img_path, mount_status).into());
    }

    let rsync_status = tokio::process::Command::new("rsync")
        .args([
            "-aHAX",
            "--delete",
            &format!("{}/", src_dir),
            &format!("{}/", mnt_dir),
        ])
        .status()
        .await;

    let umount_ok = tokio::process::Command::new("umount")
        .arg(&mnt_dir)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !umount_ok {
        warn!("umount {} failed, trying force detach", mnt_dir);
        let _ = unmount(&mnt_dir, MNT_DETACH | MNT_NOFOLLOW);
    }
    if let Err(e) = tokio::fs::remove_dir_all(&mnt_dir).await {
        warn!("remove mnt dir {} failed: {}", mnt_dir, e);
    }

    match rsync_status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(anyhow!("rsync to {} failed: {:?}", fstype, s).into()),
        Err(e) => Err(anyhow!("rsync to {}: {}", fstype, e).into()),
    }
}
