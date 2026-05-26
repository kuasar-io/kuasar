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

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use containerd_sandbox::error::Result;

use crate::{
    sandbox::TemplateLeaseMode,
    template::SnapshotType,
    vm::{DiskImageEntry, RestoreSource},
};

/// Tracks a single disk file that was moved via `rename(2)` (MovedExclusive).
/// Used to roll back the rename if the restore fails after the move.
#[derive(Debug)]
pub(crate) struct PendingMove {
    /// Where the disk currently lives (inside the sandbox base_dir).
    pub(crate) current: PathBuf,
    /// The original path inside the template snapshot dir — restored on rollback.
    pub(crate) original: PathBuf,
}

#[derive(Debug, Default)]
pub(crate) struct RestoredBlockArtifacts {
    pub(crate) disk_remaps: Vec<(String, String)>,
    pub(crate) restored_paths: Vec<PathBuf>,
    pub(crate) action: RestoreBlockAction,
    /// Non-empty only for `MovedExclusive`; each entry is a disk that was renamed and
    /// must be renamed back to `original` if the restore pipeline fails.
    pub(crate) pending_moves: Vec<PendingMove>,
}

/// Rename all moved disks back to their original template-dir paths.
///
/// Called on any restore failure after `prepare_restore_block_artifacts` has run with
/// `MovedExclusive` action.  Returns the first rename error encountered; any subsequent
/// disks are NOT rolled back.  Callers must log the error and continue — the template
/// snapshot may be left partially inconsistent, but the restore pipeline has already
/// failed at this point.
///
/// The CH process **must be stopped** before calling this function so it cannot
/// access the disk files after they are moved back.
pub(crate) async fn rollback_pending_moves(
    artifacts: &RestoredBlockArtifacts,
) -> std::io::Result<()> {
    for pm in &artifacts.pending_moves {
        tokio::fs::rename(&pm.current, &pm.original).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RestoreBlockAction {
    #[default]
    None,
    ReflinkCopied,
    PlainCopied,
    Symlinked,
    /// Continuation (Exclusive): atomically transfer disk ownership into the sandbox
    /// directory via `rename(2)`.  The template directory is invalidated after this
    /// operation — it no longer holds the disk files.  Only valid when source and
    /// destination are on the same filesystem; cross-node migration requires distributed
    /// block storage (see `StorageContinuationBackend` in the proposal).
    MovedExclusive,
}

impl RestoreBlockAction {
    pub(crate) fn as_log_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReflinkCopied => "reflink copied",
            Self::PlainCopied => "plain copied",
            Self::Symlinked => "symlinked",
            Self::MovedExclusive => "moved exclusive",
        }
    }
}

pub(crate) async fn prepare_restore_block_artifacts(
    src: &RestoreSource,
    base_dir: &Path,
) -> Result<RestoredBlockArtifacts> {
    if src.disk_images.is_empty() {
        return Ok(RestoredBlockArtifacts::default());
    }

    tokio::fs::create_dir_all(base_dir)
        .await
        .map_err(|e| anyhow!("create restore block dir {}: {}", base_dir.display(), e))?;

    let action = restore_action(src);
    let mut restored_paths = Vec::with_capacity(src.disk_images.len());
    let mut disk_remaps = Vec::with_capacity(src.disk_images.len());
    let mut pending_moves: Vec<PendingMove> = Vec::new();

    for entry in &src.disk_images {
        let src_img = snapshot_disk_path(src, entry);
        let dst_img = base_dir.join(format!("{}.img", entry.storage_id));

        match action {
            RestoreBlockAction::ReflinkCopied => {
                super::copy_img_file_reflink(&src_img, &dst_img).await?
            }
            RestoreBlockAction::PlainCopied => super::copy_img_file(&src_img, &dst_img).await?,
            RestoreBlockAction::Symlinked => tokio::fs::symlink(&src_img, &dst_img)
                .await
                .map_err(|e| anyhow!("restore disk {}: symlink: {}", entry.storage_id, e))?,
            RestoreBlockAction::MovedExclusive => {
                if let Err(e) = tokio::fs::rename(&src_img, &dst_img).await {
                    // Partial failure: roll back all renames completed so far so the
                    // template snapshot directory is left intact for the next attempt.
                    for pm in &pending_moves {
                        if let Err(re) = tokio::fs::rename(&pm.current, &pm.original).await {
                            log::warn!(
                                "restore disk: partial-move rollback failed \
                                 {} → {}: {} (template may be missing disk files)",
                                pm.current.display(),
                                pm.original.display(),
                                re
                            );
                        }
                    }
                    return Err(anyhow!(
                        "restore disk {}: rename {} → {}: {} \
                         (cross-filesystem rename not supported; use distributed block \
                         storage for cross-node Continuation migration)",
                        entry.storage_id,
                        src_img.display(),
                        dst_img.display(),
                        e
                    )
                    .into());
                }
                // Track this move so restore() can rename back on failure.
                pending_moves.push(PendingMove {
                    current: dst_img.clone(),
                    original: src_img,
                });
            }
            RestoreBlockAction::None => {}
        }

        disk_remaps.push((
            entry.device_id.clone(),
            dst_img.to_string_lossy().to_string(),
        ));
        restored_paths.push(dst_img);
    }

    Ok(RestoredBlockArtifacts {
        disk_remaps,
        restored_paths,
        action,
        pending_moves,
    })
}

fn restore_action(src: &RestoreSource) -> RestoreBlockAction {
    match &src.snapshot_type {
        // Continuation is always exclusive-consumed. Transfer disk ownership into the sandbox
        // directory via rename(2) so the template directory can be safely cleaned up without
        // leaving dangling symlinks in the sandbox.
        SnapshotType::Continuation => RestoreBlockAction::MovedExclusive,
        SnapshotType::WarmFork if src.lease_mode == TemplateLeaseMode::Shared => {
            if src.reflink_supported {
                RestoreBlockAction::ReflinkCopied
            } else {
                RestoreBlockAction::PlainCopied
            }
        }
        _ => RestoreBlockAction::Symlinked,
    }
}

fn snapshot_disk_path(src: &RestoreSource, entry: &DiskImageEntry) -> PathBuf {
    src.snapshot_dir.join(&entry.filename)
}

#[cfg(test)]
mod tests {
    use temp_dir::TempDir;

    use super::*;
    use crate::{sandbox::MemoryRestoreMode, template::SnapshotType, vm::SnapshotPathOverrides};

    fn restore_source(
        dir: &TempDir,
        snapshot_type: SnapshotType,
        lease_mode: TemplateLeaseMode,
        reflink_supported: bool,
    ) -> RestoreSource {
        RestoreSource {
            snapshot_dir: dir.path().join("snapshot"),
            work_dir: dir.path().join("work"),
            overrides: SnapshotPathOverrides {
                task_vsock: "/tmp/task.vsock".to_string(),
                console_path: "/tmp/console.log".to_string(),
            },
            snapshot_type,
            disk_images: vec![DiskImageEntry {
                storage_id: "storage-a".to_string(),
                device_id: "disk-a".to_string(),
                filename: "disks/storage-a.img".to_string(),
            }],
            storages: vec![],
            orphan_container_ids: vec![],
            memory_restore_mode: MemoryRestoreMode::Copy,
            reflink_supported,
            lease_mode,
            warm_fork_params: None,
        }
    }

    async fn write_snapshot_disk(dir: &TempDir) {
        tokio::fs::create_dir_all(dir.path().join("snapshot/disks"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/disks/storage-a.img"), b"disk")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prepare_restore_block_artifacts_copies_shared_without_reflink() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::WarmFork,
            TemplateLeaseMode::Shared,
            false,
        );

        let prepared = prepare_restore_block_artifacts(&src, &dir.path().join("restore"))
            .await
            .unwrap();

        assert_eq!(prepared.action, RestoreBlockAction::PlainCopied);
        assert_eq!(
            prepared.disk_remaps,
            vec![(
                "disk-a".to_string(),
                dir.path()
                    .join("restore/storage-a.img")
                    .to_string_lossy()
                    .to_string()
            )]
        );
        assert_eq!(
            tokio::fs::read(dir.path().join("restore/storage-a.img"))
                .await
                .unwrap(),
            b"disk"
        );
    }

    #[tokio::test]
    async fn prepare_restore_block_artifacts_symlinks_exclusive_snapshot() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::WarmFork,
            TemplateLeaseMode::Exclusive,
            false,
        );

        let prepared = prepare_restore_block_artifacts(&src, &dir.path().join("restore"))
            .await
            .unwrap();

        assert_eq!(prepared.action, RestoreBlockAction::Symlinked);
        assert!(
            tokio::fs::symlink_metadata(dir.path().join("restore/storage-a.img"))
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn prepare_restore_block_artifacts_continuation_exclusive_moves_file() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::Continuation,
            TemplateLeaseMode::Exclusive,
            false,
        );

        let restore_dir = dir.path().join("restore");
        let prepared = prepare_restore_block_artifacts(&src, &restore_dir)
            .await
            .unwrap();

        assert_eq!(prepared.action, RestoreBlockAction::MovedExclusive);
        // Disk file must now exist in the sandbox restore directory.
        assert!(
            tokio::fs::metadata(restore_dir.join("storage-a.img"))
                .await
                .is_ok(),
            "disk file not found in restore dir after MovedExclusive"
        );
        // Original snapshot disk file must no longer exist (renamed away).
        assert!(
            tokio::fs::metadata(dir.path().join("snapshot/disks/storage-a.img"))
                .await
                .is_err(),
            "original disk file should be absent after MovedExclusive rename"
        );
        // The moved file must be a regular file, not a symlink.
        let meta = tokio::fs::symlink_metadata(restore_dir.join("storage-a.img"))
            .await
            .unwrap();
        assert!(
            meta.file_type().is_file(),
            "MovedExclusive result must be a regular file, not a symlink"
        );
    }

    /// Partial rename failure must roll back already-moved disks so the template is intact.
    ///
    /// Two disk images: first rename succeeds, second fails (source absent).
    /// After the error, the first disk must be back in the snapshot dir.
    #[tokio::test]
    async fn prepare_restore_block_artifacts_partial_move_rolls_back_on_failure() {
        let dir = TempDir::new().unwrap();
        // Only create the first disk file; the second is intentionally absent.
        tokio::fs::create_dir_all(dir.path().join("snapshot/disks"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/disks/storage-a.img"), b"disk-a")
            .await
            .unwrap();
        // storage-b.img deliberately NOT created → second rename will fail.

        let src = RestoreSource {
            snapshot_dir: dir.path().join("snapshot"),
            work_dir: dir.path().join("work"),
            overrides: SnapshotPathOverrides {
                task_vsock: String::new(),
                console_path: String::new(),
            },
            snapshot_type: SnapshotType::Continuation,
            disk_images: vec![
                DiskImageEntry {
                    storage_id: "storage-a".to_string(),
                    device_id: "disk-a".to_string(),
                    filename: "disks/storage-a.img".to_string(),
                },
                DiskImageEntry {
                    storage_id: "storage-b".to_string(),
                    device_id: "disk-b".to_string(),
                    filename: "disks/storage-b.img".to_string(), // does not exist
                },
            ],
            storages: vec![],
            orphan_container_ids: vec![],
            memory_restore_mode: MemoryRestoreMode::Copy,
            reflink_supported: false,
            lease_mode: TemplateLeaseMode::Exclusive,
            warm_fork_params: None,
        };
        let restore_dir = dir.path().join("restore");
        let result = prepare_restore_block_artifacts(&src, &restore_dir).await;

        assert!(result.is_err(), "expected error on missing second disk");
        // The first disk must have been rolled back to the snapshot dir.
        assert!(
            tokio::fs::metadata(dir.path().join("snapshot/disks/storage-a.img"))
                .await
                .is_ok(),
            "first disk must be rolled back to snapshot dir after partial-move failure"
        );
        // The first disk must not be stuck in the restore dir.
        assert!(
            tokio::fs::metadata(restore_dir.join("storage-a.img"))
                .await
                .is_err(),
            "first disk must not remain in restore dir after rollback"
        );
    }

    /// MovedExclusive must populate pending_moves so restore() can roll back on failure.
    #[tokio::test]
    async fn prepare_restore_block_continuation_exclusive_records_pending_move() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::Continuation,
            TemplateLeaseMode::Exclusive,
            false,
        );
        let restore_dir = dir.path().join("restore");
        let prepared = prepare_restore_block_artifacts(&src, &restore_dir)
            .await
            .unwrap();

        assert_eq!(prepared.pending_moves.len(), 1);
        assert_eq!(
            prepared.pending_moves[0].current,
            restore_dir.join("storage-a.img")
        );
        assert_eq!(
            prepared.pending_moves[0].original,
            dir.path().join("snapshot/disks/storage-a.img")
        );
    }

    /// Non-MovedExclusive actions must not record any pending_moves.
    #[tokio::test]
    async fn prepare_restore_block_non_exclusive_no_pending_moves() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::WarmFork,
            TemplateLeaseMode::Shared,
            false,
        );
        let prepared = prepare_restore_block_artifacts(&src, &dir.path().join("restore"))
            .await
            .unwrap();
        assert!(
            prepared.pending_moves.is_empty(),
            "non-MovedExclusive must not produce pending_moves"
        );
    }

    /// rollback_pending_moves renames the disk back to the template dir.
    #[tokio::test]
    async fn rollback_pending_moves_restores_template_disk() {
        let dir = TempDir::new().unwrap();
        write_snapshot_disk(&dir).await;
        let src = restore_source(
            &dir,
            SnapshotType::Continuation,
            TemplateLeaseMode::Exclusive,
            false,
        );
        let restore_dir = dir.path().join("restore");
        let prepared = prepare_restore_block_artifacts(&src, &restore_dir)
            .await
            .unwrap();

        // Simulate a restore failure: roll back the moved disk.
        rollback_pending_moves(&prepared).await.unwrap();

        // After rollback, disk must be back in the snapshot dir.
        assert!(
            tokio::fs::metadata(dir.path().join("snapshot/disks/storage-a.img"))
                .await
                .is_ok(),
            "disk must be restored to original path after rollback"
        );
        // And must no longer exist at the restore dir.
        assert!(
            tokio::fs::metadata(restore_dir.join("storage-a.img"))
                .await
                .is_err(),
            "disk must not exist in restore dir after rollback"
        );
    }

    #[test]
    fn restore_action_continuation_exclusive_is_moved_exclusive() {
        let dir = TempDir::new().unwrap();
        let src = restore_source(
            &dir,
            SnapshotType::Continuation,
            TemplateLeaseMode::Exclusive,
            false,
        );
        assert_eq!(restore_action(&src), RestoreBlockAction::MovedExclusive);
    }
}
