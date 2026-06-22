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

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use nix::libc::MNT_DETACH;

use super::{PooledTemplate, TemplateKey, WorkloadIdentity};

/// Sentinel file written inside an entry directory after the entry is acquired.
/// Prevents crash-recovery from re-offering an already-consumed snapshot on restart.
/// Removed by `ContinuationLease::fail()` if the restore is rolled back.
const CONSUMED_MARKER: &str = "consumed";

/// Independent store for [`super::SnapshotType::Continuation`] snapshots.
///
/// Unlike `TemplatePool`, which manages Environment and WarmFork templates with shared leases,
/// water-level GC, and background refill, `ContinuationStore` is a simple per-workload registry:
///
/// - **Directory layout**: mixed depth, determined by the key structure:
///   - Auto-generated key `{pod_uid}/g{generation}` → two-level `{store_dir}/{pod_uid}/g{generation}/`
///   - User-supplied `snapshot_name` (no `/`) → one-level `{store_dir}/{snapshot_name}/`
///   - In both cases `store_dir.join(key)` yields the correct path directly.
/// - **Lookup**: O(1) filesystem stat by key — no in-memory HashMap required.
/// - **Consume semantics**: exclusive 1:1; the entry is consumed on acquire and never reused.
/// - **GC**: consumed entries are cleaned by condition-based ContinuationStore GC; pool-gc does not apply.
///
/// This enables the cross-node migration use case: the receiving node locates a continuation
/// snapshot with a single `stat(2)` without waiting for pool rehydration.
pub struct ContinuationStore {
    /// Root of the continuation store: `{base_dir}/continuation/`.
    pub store_dir: PathBuf,
}

/// RAII guard returned by [`ContinuationStore::acquire`].
///
/// - `complete()`: no-op — the consumed marker was already written at acquire time.
/// - `fail()`: removes the consumed marker so the entry can be re-acquired on the next attempt.
pub struct ContinuationLease {
    store: Arc<ContinuationStore>,
    tmpl: Option<PooledTemplate>,
}

impl ContinuationStore {
    /// Create a new store rooted at `{base_dir}/continuation/`.
    pub fn new(base_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            store_dir: base_dir.join("continuation"),
        })
    }

    /// Rehydrate from disk on sandboxer restart.
    ///
    /// Ensures the store directory exists (creates it if absent).
    /// Entries are looked up on-demand from disk rather than pre-loaded into memory.
    pub async fn load_from_disk(base_dir: PathBuf) -> Result<Arc<Self>> {
        let store_dir = base_dir.join("continuation");
        tokio::fs::create_dir_all(&store_dir).await.map_err(|e| {
            anyhow::anyhow!(
                "continuation store: could not create {}: {}",
                store_dir.display(),
                e
            )
        })?;
        Ok(Arc::new(Self { store_dir }))
    }

    /// Returns the on-disk directory for a given template key.
    ///
    /// `store_dir.join(key)` handles both layouts automatically:
    /// keys without `/` produce a one-level path; keys with `/` produce a two-level path.
    pub fn entry_dir_by_key(&self, key: &str) -> PathBuf {
        self.store_dir.join(key)
    }

    /// Returns the on-disk directory for a given workload identity.
    pub fn entry_dir(&self, identity: &WorkloadIdentity) -> PathBuf {
        self.entry_dir_by_key(
            &TemplateKey::from_workload_identity(&identity.pod_uid, identity.generation).key,
        )
    }

    /// Persist continuation snapshot metadata to disk.
    ///
    /// The VM snapshot files must already reside at `{entry_dir}/snapshot/` before this call;
    /// this method only writes `pooled_template.json`. Callers should build the snapshot_dir
    /// as `entry_dir.join("snapshot")` and pass it to `vm.snapshot()`.
    ///
    /// Requires `tmpl.workload_identity` to be set.
    pub async fn save(&self, tmpl: &PooledTemplate) -> Result<()> {
        let dir = self.entry_dir_by_key(&tmpl.key.key);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow!("continuation store: create {}: {}", dir.display(), e))?;
        tmpl.save(&dir).await.map_err(|e| {
            anyhow!(
                "continuation store: save metadata for key={}: {}",
                tmpl.key.key,
                e
            )
            .into()
        })
    }

    /// Acquire (consume) a continuation snapshot for restore by workload identity.
    ///
    /// Returns `None` if the entry does not exist or has already been consumed.
    /// Call `ContinuationLease::fail()` to undo the consume on restore failure.
    pub async fn acquire(
        self: &Arc<Self>,
        identity: &WorkloadIdentity,
    ) -> Option<ContinuationLease> {
        let key = TemplateKey::from_workload_identity(&identity.pod_uid, identity.generation).key;
        self.acquire_from_dir(&self.entry_dir_by_key(&key), &key)
            .await
    }

    async fn acquire_from_dir(
        self: &Arc<Self>,
        dir: &PathBuf,
        key: &str,
    ) -> Option<ContinuationLease> {
        if !tokio::fs::try_exists(dir).await.unwrap_or(false) {
            log::debug!("continuation store: no entry for key={}", key);
            return None;
        }

        let marker = dir.join(CONSUMED_MARKER);
        if tokio::fs::try_exists(&marker).await.unwrap_or(false) {
            log::warn!(
                "continuation store: key={} already consumed (marker at {})",
                key,
                marker.display()
            );
            return None;
        }

        let tmpl = match PooledTemplate::load(dir).await {
            Ok(t) => t,
            Err(e) => {
                log::error!("continuation store: failed to load key={}: {}", key, e);
                return None;
            }
        };

        if tmpl.snapshot_type != super::SnapshotType::Continuation {
            log::error!(
                "continuation store: key={} has unexpected snapshot_type {:?}; rejecting",
                key,
                tmpl.snapshot_type,
            );
            return None;
        }

        if tmpl.key.key != key {
            log::error!(
                "continuation store: key mismatch: requested={}, metadata contains={}; \
                 directory may be corrupted or misplaced",
                key,
                tmpl.key.key,
            );
            return None;
        }

        if let Err(e) = write_consumed_marker(&marker).await {
            log::error!(
                "continuation store: failed to write consumed marker for key={}: {}",
                key,
                e
            );
            return None;
        }

        log::info!(
            "continuation store: acquired key={} (template_id={})",
            key,
            tmpl.id
        );
        Some(ContinuationLease {
            store: Arc::clone(self),
            tmpl: Some(tmpl),
        })
    }

    /// Acquire a continuation snapshot by its template key (O(1) stat, no scan).
    pub async fn acquire_by_key(self: &Arc<Self>, key: &str) -> Option<ContinuationLease> {
        self.acquire_from_dir(&self.entry_dir_by_key(key), key)
            .await
    }

    /// Collect all template entry directories, handling both layouts:
    /// - one-level  `{store_dir}/{snapshot_name}/`       (user-supplied key, no `/`)
    /// - two-level  `{store_dir}/{pod_uid}/g{generation}/` (auto-generated key, contains `/`)
    ///
    /// A top-level directory is treated as a template entry when it contains
    /// `pooled_template.json` directly; otherwise it is treated as a pod-UID
    /// parent directory and its immediate subdirectories are collected instead.
    async fn entry_dirs(&self) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut top = match tokio::fs::read_dir(&self.store_dir).await {
            Ok(d) => d,
            Err(_) => return result,
        };
        while let Ok(Some(top_entry)) = top.next_entry().await {
            let top_path = top_entry.path();
            if !top_path.is_dir() {
                continue;
            }
            if tokio::fs::try_exists(top_path.join("pooled_template.json"))
                .await
                .unwrap_or(false)
            {
                result.push(top_path);
            } else {
                let mut sub = match tokio::fs::read_dir(&top_path).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                while let Ok(Some(sub_entry)) = sub.next_entry().await {
                    let sub_path = sub_entry.path();
                    if sub_path.is_dir()
                        && tokio::fs::try_exists(sub_path.join("pooled_template.json"))
                            .await
                            .unwrap_or(false)
                    {
                        result.push(sub_path);
                    }
                }
            }
        }
        result
    }

    /// Enumerate all available (not yet consumed) entries.
    pub async fn list(&self) -> Vec<PooledTemplate> {
        let mut results = Vec::new();
        for path in self.entry_dirs().await {
            if tokio::fs::try_exists(path.join(CONSUMED_MARKER))
                .await
                .unwrap_or(false)
            {
                continue;
            }
            match PooledTemplate::load(&path).await {
                Ok(tmpl) => results.push(tmpl),
                Err(e) => log::warn!(
                    "continuation store: failed to load {}: {}",
                    path.display(),
                    e
                ),
            }
        }
        results
    }

    /// Explicitly delete a continuation entry and all its snapshot files.
    ///
    /// Returns `true` if the entry existed and was removed, `false` if it was not found.
    /// Used by the `continuation-delete` admin command.
    pub async fn delete(&self, identity: &WorkloadIdentity) -> Result<bool> {
        let key = TemplateKey::from_workload_identity(&identity.pod_uid, identity.generation).key;
        let dir = self.entry_dir_by_key(&key);
        unmount_preserved_netns(&dir);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {
                log::info!("continuation store: deleted key={}", key);
                // Best-effort: remove the empty pod-UID parent left by the two-level layout.
                // Guard: only attempt removal when the parent is strictly inside store_dir
                // (i.e. two-level key). For one-level keys dir.parent() equals store_dir
                // itself, and we must never try to delete the store root.
                if let Some(parent) = dir.parent() {
                    if parent != self.store_dir.as_path() {
                        let _ = tokio::fs::remove_dir(parent).await;
                    }
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow!("continuation store: delete key={}: {}", key, e).into()),
        }
    }

    /// Delete consumed entries that are no longer referenced by any live sandbox.
    ///
    /// Returns the number of entry directories removed. Unconsumed entries are never removed
    /// by this method.
    pub async fn gc_orphaned_consumed(
        &self,
        active_template_ids: &HashSet<String>,
    ) -> Result<usize> {
        let mut removed = 0usize;
        for entry_path in self.entry_dirs().await {
            let marker = entry_path.join(CONSUMED_MARKER);
            if tokio::fs::metadata(&marker).await.is_err() {
                continue;
            }
            let tmpl = match PooledTemplate::load(&entry_path).await {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "continuation store GC: failed to load {}: {}",
                        entry_path.display(),
                        e
                    );
                    continue;
                }
            };
            if active_template_ids.contains(&tmpl.id) {
                continue;
            }
            unmount_preserved_netns(&entry_path);
            match tokio::fs::remove_dir_all(&entry_path).await {
                Ok(()) => {
                    removed += 1;
                    log::info!(
                        "continuation store GC: removed consumed entry {}",
                        entry_path.display()
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!(
                    "continuation store GC: remove {}: {}",
                    entry_path.display(),
                    e
                ),
            }
        }
        Ok(removed)
    }

    /// Delete a consumed entry by its template_id.
    ///
    /// Returns `true` if a matching entry was removed.
    pub async fn delete_by_template_id(&self, template_id: &str) -> Result<bool> {
        for entry_path in self.entry_dirs().await {
            let tmpl = match PooledTemplate::load(&entry_path).await {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "continuation store: failed to load {} while deleting template {}: {}",
                        entry_path.display(),
                        template_id,
                        e
                    );
                    continue;
                }
            };
            if tmpl.id != template_id {
                continue;
            }
            unmount_preserved_netns(&entry_path);
            match tokio::fs::remove_dir_all(&entry_path).await {
                Ok(()) => {
                    log::info!("continuation store: deleted template {}", template_id);
                    return Ok(true);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => {
                    return Err(anyhow!(
                        "continuation store: delete template {} at {}: {}",
                        template_id,
                        entry_path.display(),
                        e
                    )
                    .into());
                }
            }
        }
        Ok(false)
    }
}

impl ContinuationLease {
    pub fn template(&self) -> Result<&PooledTemplate> {
        self.tmpl
            .as_ref()
            .ok_or_else(|| anyhow!("ContinuationLease already finished").into())
    }

    /// Commit the restore: the consumed marker was already written at acquire time; nothing to do.
    pub async fn complete(mut self) {
        self.tmpl.take();
    }

    /// Abort the restore: remove the consumed marker so the entry can be re-acquired.
    pub async fn fail(mut self) {
        let tmpl = match self.tmpl.take() {
            Some(t) => t,
            None => return,
        };
        let marker = self
            .store
            .entry_dir_by_key(&tmpl.key.key)
            .join(CONSUMED_MARKER);
        if let Err(e) = tokio::fs::remove_file(&marker).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::error!(
                    "continuation store: failed to remove consumed marker for key={}: {}",
                    tmpl.key.key,
                    e
                );
            }
        } else {
            log::info!(
                "continuation store: released key={} (restore rolled back)",
                tmpl.key.key
            );
        }
    }
}

/// Write and fsync the consumed marker file.
///
/// fsync ensures the marker reaches disk before we return — without it a crash between
/// in-memory removal and the file appearing on disk could allow a second consume on restart.
async fn write_consumed_marker(marker: &Path) -> std::io::Result<()> {
    let file = tokio::fs::File::create(marker).await?;
    file.sync_all().await
}

/// Unmount the preserved netns bind mount inside a continuation entry directory, if present.
///
/// Called before `remove_dir_all` on any continuation entry directory.  If the entry was
/// created with a Continuation snapshot, the "preserved_netns" file is a bind mount that
/// keeps the original pod's netns alive after CNI teardown; removing the directory without
/// unmounting first would leave an orphaned kernel mount.
///
/// `unmount` returns Ok on EINVAL (not a mount point) and ENOENT (no such file), so this
/// is safe to call even when no bind mount was created (e.g. empty-netns snapshots).
fn unmount_preserved_netns(dir: &Path) {
    let preserved = dir.join("preserved_netns");
    if let Some(path_str) = preserved.to_str() {
        if let Err(e) = vmm_common::mount::unmount(path_str, MNT_DETACH) {
            log::warn!(
                "continuation store: unmount preserved_netns at {}: {}",
                preserved.display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use temp_dir::TempDir;

    use super::*;
    use crate::template::{SnapshotType, TemplateKey};

    fn make_continuation_template(pod_uid: &str, generation: u64) -> PooledTemplate {
        let mut tmpl = PooledTemplate::new(
            format!("tmpl-{}-{}", pod_uid, generation),
            TemplateKey::from_workload_identity(pod_uid, generation),
            PathBuf::from("/unused"),
            "/dev/pmem0",
            "/boot/vmlinux",
            2,
            512,
            "/run/vsock",
            "/run/console.log",
        );
        tmpl.snapshot_type = SnapshotType::Continuation;
        tmpl.workload_identity = Some(WorkloadIdentity {
            pod_uid: pod_uid.to_string(),
            generation,
        });
        tmpl
    }

    #[tokio::test]
    async fn test_entry_dir_layout() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());
        let identity = WorkloadIdentity {
            pod_uid: "abc-123".to_string(),
            generation: 2,
        };
        let dir = store.entry_dir(&identity);
        assert!(dir.ends_with("continuation/abc-123/g2"));
    }

    #[tokio::test]
    async fn test_save_and_list() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());
        let tmpl = make_continuation_template("pod-uid-1", 0);

        store.save(&tmpl).await.unwrap();

        let list = store.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, tmpl.id);
    }

    #[tokio::test]
    async fn test_acquire_writes_consumed_marker() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());
        let tmpl = make_continuation_template("pod-uid-2", 0);
        store.save(&tmpl).await.unwrap();

        let identity = tmpl.workload_identity.clone().unwrap();
        let lease = store.acquire(&identity).await.unwrap();

        // consumed marker must exist
        let marker = store.entry_dir(&identity).join(CONSUMED_MARKER);
        assert!(marker.exists(), "consumed marker must be written");

        // a second acquire must fail
        assert!(store.acquire(&identity).await.is_none());

        lease.complete().await;
    }

    #[tokio::test]
    async fn test_fail_removes_consumed_marker() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());
        let tmpl = make_continuation_template("pod-uid-3", 1);
        store.save(&tmpl).await.unwrap();

        let identity = tmpl.workload_identity.clone().unwrap();
        let lease = store.acquire(&identity).await.unwrap();

        // Simulate restore failure
        lease.fail().await;

        // consumed marker must be gone
        let marker = store.entry_dir(&identity).join(CONSUMED_MARKER);
        assert!(!marker.exists(), "consumed marker must be removed on fail");

        // re-acquire must succeed
        let lease2 = store.acquire(&identity).await;
        assert!(lease2.is_some(), "re-acquire must succeed after fail");
        lease2.unwrap().complete().await;
    }

    #[tokio::test]
    async fn test_load_from_disk_skips_consumed() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let store = ContinuationStore::new(base.clone());

        let tmpl = make_continuation_template("pod-uid-4", 0);
        store.save(&tmpl).await.unwrap();

        // Simulate crash: write consumed marker manually
        let identity = tmpl.workload_identity.clone().unwrap();
        let marker = store.entry_dir(&identity).join(CONSUMED_MARKER);
        tokio::fs::File::create(&marker).await.unwrap();

        // Reload and list — consumed entry must not appear
        let store2 = ContinuationStore::load_from_disk(base).await.unwrap();
        let list = store2.list().await;
        assert!(
            list.is_empty(),
            "consumed entry must not appear after restart"
        );
    }

    #[tokio::test]
    async fn test_gc_removes_orphaned_consumed_entries_only() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());

        let consumed = make_continuation_template("pod-gc-consumed", 1);
        store.save(&consumed).await.unwrap();
        let consumed_identity = consumed.workload_identity.clone().unwrap();
        let lease = store.acquire(&consumed_identity).await.unwrap();
        lease.complete().await;

        let available = make_continuation_template("pod-gc-available", 2);
        store.save(&available).await.unwrap();
        let available_identity = available.workload_identity.clone().unwrap();

        let active = HashSet::new();
        let removed = store.gc_orphaned_consumed(&active).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!store.entry_dir(&consumed_identity).exists());
        assert!(store.entry_dir(&available_identity).exists());
    }

    #[tokio::test]
    async fn test_delete() {
        let tmp = TempDir::new().unwrap();
        let store = ContinuationStore::new(tmp.path().to_path_buf());
        let tmpl = make_continuation_template("pod-uid-5", 0);
        store.save(&tmpl).await.unwrap();

        let identity = tmpl.workload_identity.unwrap();
        let removed = store.delete(&identity).await.unwrap();
        assert!(removed);

        // Second delete returns false (not found)
        let removed2 = store.delete(&identity).await.unwrap();
        assert!(!removed2);

        assert!(store.list().await.is_empty());
    }
}
