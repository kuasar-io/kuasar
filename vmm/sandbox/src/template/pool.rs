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
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use tokio::sync::{watch, Mutex};

#[cfg(test)]
use super::types::WorkloadIdentity;
use super::{
    metrics::TemplateMetrics,
    types::{PooledTemplate, SnapshotType, TemplateKey},
};

/// LIFO pool of pre-warmed VM templates, keyed by VM configuration.
///
/// # Design note: SnapshotGraph not yet implemented
///
/// The snapshot_restore proposal specifies a `SnapshotGraph` (nodes + edges) that models
/// cross-snapshot dependencies (base snapshots, memory deltas, block deltas, immutable
/// images, running instances) and drives GC ordering and restore sequencing from that graph.
///
/// This implementation uses a simpler flat model: `HashMap<TemplateKey, VecDeque<PooledTemplate>>`
/// plus three counters (`in_flight_restores`, `in_use_count`, `in_flight_refills`).  This is
/// sufficient for Environment and WarmFork (without deltas) but will need to be
/// replaced with the full SnapshotGraph before working-set restore, memory/block deltas, or
/// remote snapshot support are added.
///
/// Sentinel file written inside a template directory when the template is acquired from the pool.
/// Prevents crash-recovery from re-inserting already-consumed templates on restart.
/// Deleted by `release()` if the template is returned to the pool.
const CONSUMED_MARKER: &str = "consumed";

/// Pool entries are stored both in memory and on disk under `store_dir`.
/// On process restart the pool can be rehydrated via `load_from_disk()`.
pub struct TemplatePool {
    available: Mutex<HashMap<TemplateKey, VecDeque<PooledTemplate>>>,
    pub store_dir: PathBuf,
    /// Maximum Environment templates to retain per key; oldest are evicted when exceeded.
    pub environment_max_per_key: usize,
    /// Maximum WarmFork templates to retain per key; oldest are evicted when exceeded.
    pub warmfork_max_per_key: usize,
    pub metrics: Arc<TemplateMetrics>,
    /// Per-key count of background refill tasks currently running.
    ///
    /// Keyed so callers can distinguish whether an in-flight refill will actually
    /// produce a template for *their* key, avoiding spurious 15-second waits when
    /// the only in-flight refills are for a different key type (e.g. bare-VM refills
    /// while a container-snapshot key is requested).
    in_flight_refills: Mutex<HashMap<TemplateKey, usize>>,
    /// Per-template-id count of sandboxes currently restoring from it.
    ///
    /// Incremented atomically with the peek in `peek_latest_for_restore()` so GC
    /// can never observe a zero count between peek and the start of the restore.
    /// Lock order: in_flight_restores → in_use_count → available.
    in_flight_restores: Mutex<HashMap<String, usize>>,
    /// Bumped whenever a template is added or a refill task ends (success or failure).
    /// Callers waiting for a template subscribe and wake on each change.
    template_ready: watch::Sender<u64>,
    /// Reference count for Shared templates: template_id → count.
    /// Exclusive templates are not tracked (they are consumed/removed).
    in_use_count: Mutex<HashMap<String, usize>>,
    /// GC watermark across all keys before GC evicts idle Shared entries.
    pub gc_watermark: usize,
    /// Lease mode for WarmFork/Continuation templates in this pool.
    /// Shared: template stays in pool after restore, ref-counted (GC-safe).
    /// Exclusive: template is consumed on acquire (one restore per entry).
    pub lease_mode: crate::sandbox::TemplateLeaseMode,
    /// Whether reflink CoW is supported between sandboxer_working_dir and store_dir.
    /// False means restore will use plain copy instead of reflink COW.
    pub reflink_supported: bool,
}

enum RestoreLeaseKind {
    /// Environment snapshot: shared lease; template stays in pool.
    /// in_flight_restores incremented on acquire, decremented on complete/fail.
    SharedLease,
    /// WarmFork Shared mode: shared reference lease; template stays in pool.
    /// in_use_count incremented on acquire. On complete(): count kept (sandbox holds the ref
    /// until delete). On fail(): count decremented via deref() so GC can proceed.
    SharedRef,
    /// WarmFork Exclusive or Continuation: exclusive ownership; template consumed atomically.
    /// Template is removed from the pool on acquire and cannot be restored again.
    Exclusive,
}

/// A restore-time reference to a template.
///
/// Dropped via `complete()` on restore success or `fail()` on restore failure;
/// each kind releases the appropriate pool resource.
pub(crate) struct TemplateLease {
    pool: Arc<TemplatePool>,
    tmpl: Option<PooledTemplate>,
    kind: RestoreLeaseKind,
}

impl TemplateLease {
    pub(crate) fn new(pool: Arc<TemplatePool>, tmpl: PooledTemplate) -> Self {
        let kind = match tmpl.snapshot_type {
            SnapshotType::Environment => RestoreLeaseKind::SharedLease,
            SnapshotType::WarmFork => {
                // WarmFork lease mode depends on pool configuration.
                // Shared: template stays in pool, ref-counted; acquire() increments in_use_count.
                // Exclusive: template is consumed on acquire(); must be released on fail().
                if pool.lease_mode == crate::sandbox::TemplateLeaseMode::Shared {
                    RestoreLeaseKind::SharedRef
                } else {
                    RestoreLeaseKind::Exclusive
                }
            }
            SnapshotType::Continuation => {
                unreachable!(
                    "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                )
            }
        };
        Self {
            pool,
            tmpl: Some(tmpl),
            kind,
        }
    }

    pub(crate) fn template(&self) -> Result<&PooledTemplate> {
        self.tmpl
            .as_ref()
            .ok_or_else(|| anyhow!("TemplateLease already finished").into())
    }

    #[allow(clippy::expect_used)]
    pub(crate) async fn complete(mut self) {
        let tmpl = self.tmpl.take().expect("template lease already finished");
        match self.kind {
            RestoreLeaseKind::SharedLease => self.pool.end_restore(&tmpl.id).await,
            RestoreLeaseKind::SharedRef | RestoreLeaseKind::Exclusive => {}
        }
    }

    #[allow(clippy::expect_used)]
    pub(crate) async fn fail(mut self) {
        let tmpl = self.tmpl.take().expect("template lease already finished");
        match self.kind {
            RestoreLeaseKind::SharedLease => self.pool.end_restore(&tmpl.id).await,
            RestoreLeaseKind::SharedRef => self.pool.deref(&tmpl.id).await,
            RestoreLeaseKind::Exclusive => self.pool.release(tmpl).await,
        }
    }
}

impl TemplatePool {
    /// Create an empty pool backed by `store_dir`.
    pub fn new(
        store_dir: PathBuf,
        environment_max_per_key: usize,
        warmfork_max_per_key: usize,
        gc_watermark: usize,
        lease_mode: crate::sandbox::TemplateLeaseMode,
        reflink_supported: bool,
    ) -> Arc<Self> {
        let (tx, _) = watch::channel(0u64);
        Arc::new(Self {
            available: Mutex::new(HashMap::new()),
            store_dir,
            environment_max_per_key,
            warmfork_max_per_key,
            metrics: Arc::new(TemplateMetrics::default()),
            in_flight_refills: Mutex::new(HashMap::new()),
            in_flight_restores: Mutex::new(HashMap::new()),
            template_ready: tx,
            in_use_count: Mutex::new(HashMap::new()),
            gc_watermark,
            lease_mode,
            reflink_supported,
        })
    }

    /// Returns the on-disk directory for a template.
    ///
    /// Layout: `{store_dir}/environment/{id}` or `{store_dir}/warmfork/{id}`.
    pub fn template_dir(&self, tmpl: &PooledTemplate) -> PathBuf {
        match tmpl.snapshot_type {
            SnapshotType::Environment => self.store_dir.join("environment").join(&tmpl.id),
            SnapshotType::WarmFork => self.store_dir.join("warmfork").join(&tmpl.id),
            SnapshotType::Continuation => {
                unreachable!(
                    "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                )
            }
        }
    }

    /// Find the on-disk directory for a template given only its ID.
    ///
    /// Searches `environment/` first, then `warmfork/`. Returns the first path that exists,
    /// or the `environment/` path as a fallback (callers that need to create dirs use template_dir).
    pub fn find_template_dir(&self, template_id: &str) -> PathBuf {
        let env = self.store_dir.join("environment").join(template_id);
        if env.exists() {
            return env;
        }
        let wf = self.store_dir.join("warmfork").join(template_id);
        if wf.exists() {
            return wf;
        }
        env
    }

    /// Scan `store_dir` on startup and reload any previously persisted templates.
    ///
    /// Scans both `environment/` and `warmfork/` subdirectories.
    pub async fn load_from_disk(
        store_dir: PathBuf,
        environment_max_per_key: usize,
        warmfork_max_per_key: usize,
        gc_watermark: usize,
        lease_mode: crate::sandbox::TemplateLeaseMode,
        reflink_supported: bool,
    ) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(&store_dir).await.map_err(|e| {
            anyhow::anyhow!(
                "template pool: could not create {}: {}",
                store_dir.display(),
                e
            )
        })?;
        let pool = Self::new(
            store_dir.clone(),
            environment_max_per_key,
            warmfork_max_per_key,
            gc_watermark,
            lease_mode,
            reflink_supported,
        );
        // Scan environment/ and warmfork/ subdirectories.
        for subdir in &["environment", "warmfork"] {
            let kind_dir = store_dir.join(subdir);
            let mut entries = match tokio::fs::read_dir(&kind_dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!("template pool: read {}: {}", kind_dir.display(), e);
                    continue;
                }
            };
            loop {
                let entry = match entries.next_entry().await {
                    Ok(Some(e)) => e,
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!(
                            "template pool: error iterating {}: {}",
                            kind_dir.display(),
                            e
                        );
                        break;
                    }
                };
                let dir = entry.path();
                if tokio::fs::try_exists(dir.join(CONSUMED_MARKER))
                    .await
                    .unwrap_or(false)
                {
                    log::info!(
                        "template pool: skipping consumed template at {} (restart after crash)",
                        dir.display()
                    );
                    continue;
                }
                match PooledTemplate::load(&dir).await {
                    Ok(tmpl) => {
                        let mut map = pool.available.lock().await;
                        map.entry(tmpl.key.clone()).or_default().push_back(tmpl);
                    }
                    Err(e) => {
                        log::warn!(
                            "template pool: failed to load template from {}: {}",
                            dir.display(),
                            e
                        );
                    }
                }
            }
        }
        Ok(pool)
    }

    /// Add a template to the pool, persisting it to disk.
    ///
    /// If this key already has `max_per_key` entries, the oldest one is evicted.
    /// For Shared mode, if pool water level is exceeded, oldest ref_count=0 template is evicted.
    /// Returns error if eviction fails (all Shared templates have ref_count>0).
    pub async fn add(&self, tmpl: PooledTemplate) -> Result<()> {
        // Create the staging directory early so Step 1/2 cleanup paths can remove it.
        // pooled_template.json is written only after both checks pass (Bug 3.3 fix).
        let tmpl_dir = self.template_dir(&tmpl);
        tokio::fs::create_dir_all(&tmpl_dir)
            .await
            .map_err(|e| anyhow!("create template dir {}: {}", tmpl_dir.display(), e))?;

        // ========================================
        // Step 1: Check global water level
        // Lock order: in_use_count → available (consistent with GC and peek_with_ref).
        // All async I/O is performed outside the lock scope.
        // ========================================
        {
            let total = self.total_depth().await;
            if total >= self.gc_watermark {
                // Collect the oldest idle Shared template while holding both locks.
                let oldest_candidate: Option<PooledTemplate> = {
                    // Lock order: in_flight_restores → in_use_count → available
                    let restores = self.in_flight_restores.lock().await;
                    let counts = self.in_use_count.lock().await;
                    let available = self.available.lock().await;
                    let mut candidate: Option<PooledTemplate> = None;
                    for queue in available.values() {
                        for t in queue.iter() {
                            if restores.contains_key(&t.id) {
                                continue;
                            }
                            let evictable = match &t.snapshot_type {
                                // Environment is evictable only when no running sandbox holds a
                                // non-Copy memory-mode pin (in_use_count == 0).
                                SnapshotType::Environment => counts.get(&t.id).is_none(),
                                SnapshotType::WarmFork => {
                                    self.lease_mode == crate::sandbox::TemplateLeaseMode::Shared
                                        && counts.get(&t.id).is_none()
                                }
                                SnapshotType::Continuation => unreachable!(
                                    "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                                ),
                            };
                            if evictable
                                && candidate
                                    .as_ref()
                                    .is_none_or(|o| t.created_at_secs < o.created_at_secs)
                            {
                                candidate = Some(t.clone());
                            }
                        }
                    }
                    candidate
                    // all locks released here
                };

                match oldest_candidate {
                    Some(candidate) => {
                        // Delete from disk first; only remove from pool on success.
                        let old_dir = self.template_dir(&candidate);
                        if let Err(e) = tokio::fs::remove_dir_all(&old_dir).await {
                            log::warn!(
                                "water level evict {}: remove dir failed: {} — keeping in pool",
                                candidate.id,
                                e
                            );
                            if let Err(ce) = tokio::fs::remove_dir_all(&tmpl_dir).await {
                                log::warn!(
                                    "cleanup new template dir {}: {}",
                                    tmpl_dir.display(),
                                    ce
                                );
                            }
                            return Err(anyhow!(
                                "pool water level exceeded and eviction of {} failed: {}",
                                candidate.id,
                                e
                            )
                            .into());
                        }
                        self.available
                            .lock()
                            .await
                            .values_mut()
                            .for_each(|q| q.retain(|t| t.id != candidate.id));
                        log::info!(
                            "evicted oldest shared template {} (created_at_secs={}, key={}) to free water level space",
                            candidate.id, candidate.created_at_secs, candidate.key.key
                        );
                    }
                    None => {
                        if let Err(ce) = tokio::fs::remove_dir_all(&tmpl_dir).await {
                            log::warn!("cleanup new template dir {}: {}", tmpl_dir.display(), ce);
                        }
                        return Err(anyhow!(
                            "pool water level exceeded: total={} >= gc_watermark={}, \
                            no evictable template found (all in use or being restored). \
                            Wait for sandboxes to be deleted or increase gc_watermark.",
                            total,
                            self.gc_watermark
                        )
                        .into());
                    }
                }
            }
        }

        // ========================================
        // Step 2: Check per-kind max_per_key depth.
        // Environment and WarmFork templates have separate limits.
        // Lock order: in_use_count → available.
        // Eviction decision is made under locks; async I/O runs outside.
        // ========================================
        let kind_max_per_key = match tmpl.snapshot_type {
            SnapshotType::Environment => self.environment_max_per_key,
            SnapshotType::WarmFork => self.warmfork_max_per_key,
            SnapshotType::Continuation => unreachable!(
                "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
            ),
        };
        let eviction: Option<(PooledTemplate, bool)> = {
            // Lock order: in_flight_restores → in_use_count → available
            let restores = self.in_flight_restores.lock().await;
            let counts = self.in_use_count.lock().await;
            let map = self.available.lock().await;
            map.get(&tmpl.key).and_then(|q| {
                if q.len() >= kind_max_per_key {
                    q.front().map(|oldest| {
                        let not_restoring = !restores.contains_key(&oldest.id);
                        let not_pinned = counts.get(&oldest.id).is_none();
                        // Evictable only when no sandbox holds an in_use_count ref:
                        // Environment non-Copy and WarmFork (Shared) pin via in_use_count
                        // while the backing sandbox is alive.
                        let can_evict = not_restoring && not_pinned;
                        (oldest.clone(), can_evict)
                    })
                } else {
                    None
                }
            })
            // all locks released here
        };

        if let Some((oldest, can_evict)) = eviction {
            if !can_evict {
                if let Err(ce) = tokio::fs::remove_dir_all(&tmpl_dir).await {
                    log::warn!("cleanup new template dir {}: {}", tmpl_dir.display(), ce);
                }
                return Err(anyhow!(
                    "max_per_key={} exceeded for key '{}': oldest template {} cannot be evicted \
                    (in use, being restored, or ref_count>0). \
                    Wait for sandboxes to be deleted or increase the relevant max_per_key.",
                    kind_max_per_key,
                    tmpl.key.key,
                    oldest.id
                )
                .into());
            }
            // Delete from disk first; only remove from pool on success.
            let old_dir = self.template_dir(&oldest);
            if let Err(e) = tokio::fs::remove_dir_all(&old_dir).await {
                log::warn!(
                    "max_per_key evict {}: remove dir failed: {} — keeping in pool",
                    oldest.id,
                    e
                );
                if let Err(ce) = tokio::fs::remove_dir_all(&tmpl_dir).await {
                    log::warn!("cleanup new template dir {}: {}", tmpl_dir.display(), ce);
                }
                return Err(anyhow!("eviction of {} failed: {}", oldest.id, e).into());
            }
            if let Some(q) = self.available.lock().await.get_mut(&tmpl.key) {
                q.retain(|t| t.id != oldest.id);
            }
            log::info!(
                "evicted oldest template {} for key '{}' (max_per_key={} exceeded)",
                oldest.id,
                oldest.key.key,
                kind_max_per_key
            );
        }

        // Persist to disk and insert into pool.
        tmpl.save(&tmpl_dir).await?;
        let key_depth = {
            let mut map = self.available.lock().await;
            let queue = map.entry(tmpl.key.clone()).or_default();
            queue.push_back(tmpl.clone());
            queue.len()
        };
        self.template_ready.send_modify(|v| *v += 1);

        let total = self.total_depth().await;
        log::info!(
            "template {} added to pool (key={}, snapshot_type={:?}, total={}, gc_watermark={}, key_depth={})",
            tmpl.id,
            tmpl.key.key,
            tmpl.snapshot_type,
            total,
            self.gc_watermark,
            key_depth
        );
        Ok(())
    }

    /// Signal that a background refill task is starting for `key`.
    ///
    /// Must be paired with `end_refill(key)` regardless of outcome so that
    /// `wait_for_template` can detect when all relevant refills are exhausted.
    pub async fn begin_refill(&self, key: &TemplateKey) {
        let mut map = self.in_flight_refills.lock().await;
        *map.entry(key.clone()).or_default() += 1;
    }

    /// Signal that a background refill task for `key` has finished (success or failure).
    ///
    /// Wakes any callers blocked in `wait_for_template` so they can either acquire
    /// the new template or discover that all refills for their key are exhausted.
    pub async fn end_refill(&self, key: &TemplateKey) {
        {
            let mut map = self.in_flight_refills.lock().await;
            if let Some(count) = map.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    map.remove(key);
                }
            }
        }
        self.template_ready.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Number of background refill tasks currently in progress for `key`.
    ///
    /// Use this instead of the total in-flight count to avoid waiting for refills
    /// that target a different key (e.g. bare-VM refills while a container-snapshot
    /// key is requested).
    pub async fn in_flight_count_for_key(&self, key: &TemplateKey) -> usize {
        self.in_flight_refills
            .lock()
            .await
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Total number of background refill tasks currently in progress across all keys.
    pub async fn in_flight_count(&self) -> usize {
        self.in_flight_refills.lock().await.values().sum()
    }

    /// Block until a template for `key` is available or `timeout` elapses.
    ///
    /// Returns `None` immediately when the last in-flight refill *for this key* finishes
    /// without producing a matching template, so the caller can fall back to cold start
    /// without waiting out the full timeout.
    pub async fn wait_for_template(
        &self,
        key: &TemplateKey,
        timeout: Duration,
    ) -> Option<PooledTemplate> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.template_ready.subscribe();
        // Mark the current version as seen so the first real change triggers rx.changed().
        let _ = rx.borrow_and_update();

        loop {
            if let Some(tmpl) = self.acquire(key).await {
                return Some(tmpl);
            }
            // All refills for this key are done and nothing in pool — give up immediately.
            if self.in_flight_count_for_key(key).await == 0 {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() { return None; }
                }
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }

    /// Wait for a Shared template to become available, then peek it (increment ref_count).
    ///
    /// Mirrors `wait_for_template` for the Shared mode path: waits up to `timeout` for a
    /// refill in progress to complete, then calls `peek_with_ref` instead of `acquire`.
    pub async fn wait_and_peek_with_ref(
        &self,
        key: &TemplateKey,
        timeout: Duration,
    ) -> Option<PooledTemplate> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.template_ready.subscribe();
        let _ = rx.borrow_and_update();

        loop {
            if let Some(tmpl) = self.peek_with_ref(key).await {
                return Some(tmpl);
            }
            if self.in_flight_count_for_key(key).await == 0 {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() { return None; }
                }
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }

    /// Acquire the latest template for `key` using per-type lease semantics.
    ///
    /// - Environment: peek + increment in_flight_restores (template stays in pool).
    /// - WarmFork Shared: peek + increment in_use_count (template stays in pool).
    /// - WarmFork Exclusive: consume via acquire() (template removed, consumed marker written).
    pub async fn acquire_for_restore(&self, key: &TemplateKey) -> Option<PooledTemplate> {
        let snapshot_type = {
            let available = self.available.lock().await;
            available
                .get(key)
                .and_then(|q| q.back())
                .map(|t| t.snapshot_type.clone())
        }?;

        match snapshot_type {
            SnapshotType::Environment => self.peek_latest_for_restore(key).await,
            SnapshotType::WarmFork => {
                if self.lease_mode == crate::sandbox::TemplateLeaseMode::Shared {
                    self.peek_with_ref(key).await
                } else {
                    self.acquire(key).await
                }
            }
            SnapshotType::Continuation => unreachable!(
                "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
            ),
        }
    }

    /// Wait for any template matching `key`, then acquire it according to its
    /// own metadata. This avoids using TOML pool defaults to interpret an
    /// already-persisted template.
    pub async fn wait_and_acquire_for_restore(
        &self,
        key: &TemplateKey,
        timeout: Duration,
    ) -> Option<PooledTemplate> {
        let deadline = Instant::now() + timeout;
        let mut rx = self.template_ready.subscribe();
        let _ = rx.borrow_and_update();

        loop {
            if let Some(tmpl) = self.acquire_for_restore(key).await {
                return Some(tmpl);
            }
            if self.in_flight_count_for_key(key).await == 0 {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                result = rx.changed() => {
                    if result.is_err() { return None; }
                }
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }

    /// Acquire a specific template by id using the sharing semantics recorded
    /// in the template metadata itself. Used by the admin restore API where the
    /// caller chooses an exact template instead of a pool key.
    pub async fn acquire_by_id_for_restore(&self, template_id: &str) -> Option<PooledTemplate> {
        let snapshot_type = {
            let available = self.available.lock().await;
            available
                .values()
                .flat_map(|q| q.iter())
                .find(|t| t.id == template_id)
                .map(|t| t.snapshot_type.clone())
        }?;

        match snapshot_type {
            SnapshotType::Environment => self.peek_by_id_for_restore(template_id).await,
            SnapshotType::WarmFork => {
                if self.lease_mode == crate::sandbox::TemplateLeaseMode::Shared {
                    self.peek_by_id_with_ref(template_id).await
                } else {
                    self.acquire_by_id(template_id).await
                }
            }
            SnapshotType::Continuation => unreachable!(
                "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
            ),
        }
    }

    async fn peek_by_id_for_restore(&self, template_id: &str) -> Option<PooledTemplate> {
        let mut restores = self.in_flight_restores.lock().await;
        let available = self.available.lock().await;
        let tmpl = available
            .values()
            .flat_map(|q| q.iter())
            .find(|t| t.id == template_id)
            .cloned()?;
        *restores.entry(tmpl.id.clone()).or_insert(0) += 1;
        log::info!(
            "peek_by_id_for_restore: template {} in_flight_restores incremented",
            tmpl.id
        );
        Some(tmpl)
    }

    async fn peek_by_id_with_ref(&self, template_id: &str) -> Option<PooledTemplate> {
        let mut counts = self.in_use_count.lock().await;
        let available = self.available.lock().await;
        let tmpl = available
            .values()
            .flat_map(|q| q.iter())
            .find(|t| t.id == template_id)?;

        if self.lease_mode != crate::sandbox::TemplateLeaseMode::Shared {
            log::warn!(
                "peek_by_id_with_ref called on non-Shared pool (template {})",
                template_id
            );
            return None;
        }

        *counts.entry(tmpl.id.clone()).or_insert(0) += 1;
        let result = tmpl.clone();
        log::info!(
            "peek_by_id_with_ref: template {} ref_count incremented",
            result.id
        );
        Some(result)
    }

    /// Write and fsync the consumed marker file.
    ///
    /// fsync is required so that the marker survives a crash between the in-memory pool
    /// removal (which makes the template unavailable) and the file appearing on disk (which
    /// allows crash-recovery to skip re-inserting an already-consumed Exclusive template).
    /// Without fsync the marker might not reach disk before the next power failure, letting
    /// the template be re-loaded into the pool on restart and consumed a second time.
    async fn write_consumed_marker(marker: &std::path::Path) -> std::io::Result<()> {
        let file = tokio::fs::File::create(marker).await?;
        file.sync_all().await
    }

    async fn acquire_by_id(&self, template_id: &str) -> Option<PooledTemplate> {
        let tmpl = {
            let mut map = self.available.lock().await;
            let mut found = None;
            for queue in map.values_mut() {
                if let Some(pos) = queue.iter().position(|t| t.id == template_id) {
                    found = queue.remove(pos);
                    break;
                }
            }
            found
        }?;
        let marker = self.template_dir(&tmpl).join(CONSUMED_MARKER);
        if let Err(e) = Self::write_consumed_marker(&marker).await {
            log::warn!(
                "template {}: failed to write consumed marker {}: {} — returning to pool",
                tmpl.id,
                marker.display(),
                e
            );
            // Put the template back so the next caller can retry.
            // Correctness: an Exclusive template that is removed from the pool but whose
            // consumed marker is never written would be silently lost on process restart,
            // allowing a second restore from the same snapshot.
            let mut map = self.available.lock().await;
            map.entry(tmpl.key.clone()).or_default().push_back(tmpl);
            return None;
        }
        Some(tmpl)
    }

    pub async fn acquire(&self, key: &TemplateKey) -> Option<PooledTemplate> {
        let tmpl = {
            let mut map = self.available.lock().await;
            map.get_mut(key)?.pop_back()
        }?;
        let marker = self.template_dir(&tmpl).join(CONSUMED_MARKER);
        if let Err(e) = Self::write_consumed_marker(&marker).await {
            log::warn!(
                "template {}: failed to write consumed marker {}: {} — returning to pool",
                tmpl.id,
                marker.display(),
                e
            );
            // Put the template back so the next caller can retry.
            let mut map = self.available.lock().await;
            map.entry(tmpl.key.clone()).or_default().push_back(tmpl);
            return None;
        }
        Some(tmpl)
    }

    /// Return a template to the pool (e.g. after a failed restore).
    ///
    /// Removes the `consumed` marker so the template is eligible for restart recovery again.
    pub async fn release(&self, tmpl: PooledTemplate) {
        let marker = self.template_dir(&tmpl).join(CONSUMED_MARKER);
        if let Err(e) = tokio::fs::remove_file(&marker).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "template {}: failed to remove consumed marker {}: {}",
                    tmpl.id,
                    marker.display(),
                    e
                );
            }
        }
        let mut map = self.available.lock().await;
        map.entry(tmpl.key.clone()).or_default().push_back(tmpl);
    }

    /// Number of available templates for the given key.
    pub async fn depth(&self, key: &TemplateKey) -> usize {
        self.available
            .lock()
            .await
            .get(key)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    /// Peek a template for Shared mode (no acquire, no consumed marker).
    /// The template remains in the pool for concurrent sandbox use.
    /// Increments reference count to track how many sandboxes are using it.
    /// Returns None if pool empty or template is Exclusive (cannot peek).
    pub async fn peek_with_ref(&self, key: &TemplateKey) -> Option<PooledTemplate> {
        // Lock order: in_use_count → available (consistent with add() and gc_if_needed).
        let mut counts = self.in_use_count.lock().await;
        let map = self.available.lock().await;
        let tmpl = map.get(key)?.back()?;

        if self.lease_mode != crate::sandbox::TemplateLeaseMode::Shared {
            log::warn!(
                "peek_with_ref called on non-Shared pool (template {}, key={}), use acquire() instead",
                tmpl.id,
                key.key
            );
            return None;
        }

        *counts.entry(tmpl.id.clone()).or_insert(0) += 1;
        let result = tmpl.clone();
        log::info!(
            "peek_with_ref: template {} (key={}) ref_count incremented",
            result.id,
            key.key
        );
        Some(result)
    }

    /// Atomically peek the latest Environment template AND increment its in-flight restore counter.
    ///
    /// Both operations happen under the same pair of locks so GC cannot observe a window
    /// where the template exists in the pool but `in_flight_restores == 0`.
    /// Lock order: in_flight_restores → available.
    /// Returns None if the pool is empty or the key is not found.
    pub async fn peek_latest_for_restore(&self, key: &TemplateKey) -> Option<PooledTemplate> {
        let mut restores = self.in_flight_restores.lock().await;
        let available = self.available.lock().await;
        let tmpl = available.get(key).and_then(|q| q.back().cloned())?;
        *restores.entry(tmpl.id.clone()).or_insert(0) += 1;
        log::info!(
            "peek_latest_for_restore: template {} (key={}) in_flight_restores incremented",
            tmpl.id,
            key.key
        );
        Some(tmpl)
    }

    /// Decrement the in-flight restore counter for a Environment template.
    ///
    /// Must be called after every restore attempt (success or failure), including
    /// snapshot-file validation failures that occur before the semaphore is acquired.
    /// Triggers `gc_if_needed` when the count drops to zero.
    pub async fn end_restore(&self, template_id: &str) {
        let trigger_gc = {
            let mut restores = self.in_flight_restores.lock().await;
            match restores.get_mut(template_id) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    if *count == 0 {
                        restores.remove(template_id);
                        log::info!("template {} in_flight_restores dropped to 0", template_id);
                        true
                    } else {
                        false
                    }
                }
                _ => {
                    log::warn!(
                        "end_restore called on template {} with count=0 or missing (double-end?)",
                        template_id
                    );
                    false
                }
            }
            // lock released here
        };
        if trigger_gc {
            self.gc_if_needed().await;
        }
    }

    /// Decrement reference count for a Shared template.
    /// Called when sandbox is deleted or restore fails.
    /// Triggers water level GC when ref_count drops to 0.
    pub async fn deref(&self, template_id: &str) {
        let trigger_gc = {
            let mut counts = self.in_use_count.lock().await;
            match counts.get_mut(template_id) {
                None => {
                    log::warn!(
                        "deref on unknown template {} (not in ref_count map — double-deref?)",
                        template_id
                    );
                    false
                }
                Some(count) if *count == 0 => {
                    log::warn!(
                        "deref on template {} with ref_count already 0 (double-deref?)",
                        template_id
                    );
                    false
                }
                Some(count) => {
                    *count -= 1;
                    if *count == 0 {
                        counts.remove(template_id);
                        log::info!("template {} ref_count dropped to 0", template_id);
                        true
                    } else {
                        false
                    }
                }
            }
            // lock released here
        };
        if trigger_gc {
            self.gc_if_needed().await;
        }
    }

    /// Increment the runtime ref-count for a template that must stay pinned while
    /// a sandbox is running.
    ///
    /// Used in two contexts:
    /// - Environment sandbox in non-Copy mode: snapshot files are the live backing store;
    ///   the ref is held until delete() and released by deref().
    /// - WarmFork Shared sandbox: the SharedRef lease holds this ref for the full sandbox
    ///   lifetime, regardless of memory_restore_mode.
    pub async fn ref_template(&self, template_id: &str) {
        let mut counts = self.in_use_count.lock().await;
        *counts.entry(template_id.to_string()).or_insert(0) += 1;
        log::info!("template {} in_use_count incremented", template_id);
    }

    /// Rebuild runtime ref-counts from recovered sandbox dumps.
    ///
    /// The sandbox dump is the persisted lease record for a running sandbox. On
    /// sandboxer restart, in-memory refcounts are empty, so they must be rebuilt
    /// before GC is allowed to evict templates that are still backing running VMs.
    ///
    /// `shared_container_ids` — template IDs held by WarmFork Shared sandboxes via their
    /// SharedRef lease in_use_count (independent of memory_restore_mode).
    /// `env_non_copy_pinned_ids` — template IDs held by Environment sandboxes in non-Copy mode
    /// whose snapshot files are still the live backing store for the running VM.
    pub async fn rebuild_active_refs(
        &self,
        shared_container_ids: &[String],
        env_non_copy_pinned_ids: &[String],
    ) {
        let mut counts = self.in_use_count.lock().await;
        counts.clear();
        for id in shared_container_ids
            .iter()
            .chain(env_non_copy_pinned_ids.iter())
        {
            *counts.entry(id.clone()).or_insert(0) += 1;
        }
        let total = shared_container_ids.len() + env_non_copy_pinned_ids.len();
        if total > 0 {
            log::info!(
                "template pool: rebuilt {} active template reference(s) ({} shared-lease, {} env-non-copy-pinned)",
                total,
                shared_container_ids.len(),
                env_non_copy_pinned_ids.len(),
            );
        }
    }

    /// Garbage collect pool entries when total depth exceeds water level.
    ///
    /// Eligible candidates:
    ///   - Environment templates with in_flight_restores == 0
    ///   - Shared container templates with ref_count == 0
    ///   - Exclusive container templates are never in the pool (consumed by acquire())
    ///
    /// Called from `deref()` (warmfork ref_count → 0) and `end_restore()` (Environment restore done).
    pub(crate) async fn gc_if_needed(&self) {
        let total = self.total_depth().await;
        if total <= self.gc_watermark {
            return;
        }

        log::info!(
            "pool water level exceeded: total={} > gc_watermark={}, triggering GC",
            total,
            self.gc_watermark
        );

        // Collect candidates under all three locks (order: in_flight_restores → in_use_count → available).
        // All locks are released before any async I/O.
        let mut candidates: Vec<PooledTemplate> = {
            let restores = self.in_flight_restores.lock().await;
            let counts = self.in_use_count.lock().await;
            let available = self.available.lock().await;

            available
                .values()
                .flat_map(|q| q.iter())
                .filter(|t| {
                    if restores.contains_key(&t.id) {
                        return false;
                    }
                    // Evict only when no sandbox has an active pin in in_use_count.
                    // Environment non-Copy and WarmFork (Shared) pin via in_use_count
                    // for the sandbox lifetime.
                    if counts.contains_key(&t.id) {
                        return false;
                    }
                    // Exclusive WarmFork templates are consumed on restore, never GC-eligible.
                    !(matches!(&t.snapshot_type, SnapshotType::WarmFork)
                        && self.lease_mode != crate::sandbox::TemplateLeaseMode::Shared)
                })
                .cloned()
                .collect()
            // all locks released here
        };

        candidates.sort_by_key(|t| t.created_at_secs);

        let to_delete = total.saturating_sub(self.gc_watermark);
        let mut actually_deleted = 0usize;

        for tmpl in candidates.iter().take(to_delete) {
            let dir = self.template_dir(tmpl);
            match tokio::fs::remove_dir_all(&dir).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!(
                        "GC: failed to remove template dir {} — keeping in pool: {}",
                        tmpl.id,
                        e
                    );
                    continue;
                }
                Err(_) => {
                    log::info!(
                        "GC: template dir {} already gone, removing from pool",
                        tmpl.id
                    );
                }
                Ok(()) => {}
            }
            self.available
                .lock()
                .await
                .values_mut()
                .for_each(|q| q.retain(|t| t.id != tmpl.id));
            log::info!(
                "GC removed template {} (snapshot_type={:?}, created_at_secs={}, key={})",
                tmpl.id,
                tmpl.snapshot_type,
                tmpl.created_at_secs,
                tmpl.key.key
            );
            actually_deleted += 1;
        }

        let new_total = self.total_depth().await;
        log::info!(
            "pool GC completed: removed={}, total {} → {} (gc_watermark={})",
            actually_deleted,
            total,
            new_total,
            self.gc_watermark
        );
    }

    /// Total number of available templates across all keys.
    pub async fn total_depth(&self) -> usize {
        self.available.lock().await.values().map(|q| q.len()).sum()
    }

    /// Number of distinct keys that have at least one available template.
    pub async fn key_count(&self) -> usize {
        self.available
            .lock()
            .await
            .values()
            .filter(|q| !q.is_empty())
            .count()
    }

    /// Returns per-key depth snapshot for status reporting.
    ///
    /// The returned map contains only keys with at least one available template.
    pub async fn depth_by_key(&self) -> Vec<(String, usize)> {
        self.available
            .lock()
            .await
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(k, q)| (k.key.clone(), q.len()))
            .collect()
    }

    /// Returns template counts broken down by snapshot type.
    pub async fn depth_by_type(&self) -> Vec<(String, usize)> {
        let mut environment = 0usize;
        let mut warm_fork = 0usize;
        for q in self.available.lock().await.values() {
            for t in q {
                match t.snapshot_type {
                    SnapshotType::Environment => environment += 1,
                    SnapshotType::WarmFork => warm_fork += 1,
                    SnapshotType::Continuation => {}
                }
            }
        }
        vec![
            ("environment".to_string(), environment),
            ("warm_fork".to_string(), warm_fork),
        ]
    }

    /// Remove a single template by ID from pool and disk.
    ///
    /// `expected_kind` is a safety cross-check: returns an error if the template's
    /// snapshot_type does not match. Returns `false` if no template with that ID
    /// exists in the pool, `Err` if it is currently in use or the kind mismatches.
    pub async fn remove_by_id(
        &self,
        template_id: &str,
        expected_kind: &SnapshotType,
    ) -> Result<bool> {
        let tmpl = {
            let restores = self.in_flight_restores.lock().await;
            let counts = self.in_use_count.lock().await;
            let available = self.available.lock().await;
            let found = available
                .values()
                .flat_map(|q| q.iter())
                .find(|t| t.id == template_id)
                .cloned();
            match found {
                None => return Ok(false),
                Some(t) => {
                    if std::mem::discriminant(&t.snapshot_type)
                        != std::mem::discriminant(expected_kind)
                    {
                        return Err(anyhow!(
                            "template {} has kind {:?}, not {:?}",
                            template_id,
                            t.snapshot_type,
                            expected_kind
                        )
                        .into());
                    }
                    if restores.contains_key(&t.id) {
                        return Err(anyhow!(
                            "template {} is currently being restored, cannot remove",
                            template_id
                        )
                        .into());
                    }
                    if counts.contains_key(&t.id) {
                        return Err(anyhow!(
                            "template {} is in use by a running sandbox, cannot remove",
                            template_id
                        )
                        .into());
                    }
                    t
                }
            }
        };

        let dir = self.template_dir(&tmpl);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!(
                    "remove_by_id: template dir {} already gone, removing from pool",
                    template_id
                );
            }
            Err(e) => {
                return Err(
                    anyhow!("failed to remove template dir for {}: {}", template_id, e).into(),
                );
            }
        }

        self.available
            .lock()
            .await
            .values_mut()
            .for_each(|q| q.retain(|t| t.id != template_id));
        log::info!(
            "remove_by_id: removed template {} ({:?}) from pool",
            template_id,
            expected_kind
        );
        Ok(true)
    }

    /// Return a snapshot of all available templates currently managed by the pool.
    pub async fn list_templates(&self) -> Vec<PooledTemplate> {
        let mut templates: Vec<_> = self
            .available
            .lock()
            .await
            .values()
            .flat_map(|q| q.iter().cloned())
            .collect();
        templates.sort_by(|a, b| a.id.cmp(&b.id));
        templates
    }

    /// Find an available template by ID without acquiring or pinning it.
    pub async fn get_template(&self, template_id: &str) -> Option<PooledTemplate> {
        self.available
            .lock()
            .await
            .values()
            .flat_map(|q| q.iter())
            .find(|t| t.id == template_id)
            .cloned()
    }

    pub async fn active_shared_refs(&self) -> Vec<(String, usize)> {
        let mut refs: Vec<_> = self
            .in_use_count
            .lock()
            .await
            .iter()
            .map(|(id, count)| (id.clone(), *count))
            .collect();
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        refs
    }

    pub async fn in_flight_restores_by_template(&self) -> Vec<(String, usize)> {
        let mut restores: Vec<_> = self
            .in_flight_restores
            .lock()
            .await
            .iter()
            .map(|(id, count)| (id.clone(), *count))
            .collect();
        restores.sort_by(|a, b| a.0.cmp(&b.0));
        restores
    }

    pub async fn gc_blocked_templates(&self) -> Vec<(String, String)> {
        let restores = self.in_flight_restores.lock().await;
        let counts = self.in_use_count.lock().await;
        let available = self.available.lock().await;
        let mut blocked = Vec::new();
        for tmpl in available.values().flat_map(|q| q.iter()) {
            if restores.contains_key(&tmpl.id) {
                blocked.push((tmpl.id.clone(), "restore_in_flight".to_string()));
            } else if counts.contains_key(&tmpl.id) {
                blocked.push((tmpl.id.clone(), "shared_refcount".to_string()));
            }
        }
        blocked.sort_by(|a, b| a.0.cmp(&b.0));
        blocked
    }

    /// Force-evict templates of the given `snapshot_type`, keeping at most `target_depth` total
    /// for all keys of that type.  Oldest templates (lowest `created_at_secs`) are evicted first.
    ///
    /// Returns the number of templates that were actually removed from pool and disk.
    pub async fn evict_by_kind(&self, kind: &SnapshotType, target_depth: usize) -> usize {
        // Collect all templates of the requested snapshot_type, with their disk paths.
        let mut candidates: Vec<PooledTemplate> = {
            // Lock order: in_flight_restores -> in_use_count -> available
            let restores = self.in_flight_restores.lock().await;
            let counts = self.in_use_count.lock().await;
            let available = self.available.lock().await;
            available
                .values()
                .flat_map(|q| q.iter())
                .filter(|t| {
                    if std::mem::discriminant(&t.snapshot_type) != std::mem::discriminant(kind)
                        || restores.contains_key(&t.id)
                    {
                        return false;
                    }
                    match &t.snapshot_type {
                        // Environment non-Copy sandboxes pin in_use_count for their lifetime;
                        // evicting the snapshot dir while they're running would corrupt them.
                        SnapshotType::Environment => !counts.contains_key(&t.id),
                        SnapshotType::WarmFork => {
                            self.lease_mode != crate::sandbox::TemplateLeaseMode::Shared
                                || !counts.contains_key(&t.id)
                        }
                        SnapshotType::Continuation => unreachable!(
                            "Continuation snapshots are managed by ContinuationStore, not TemplatePool"
                        ),
                    }
                })
                .cloned()
                .collect()
        };

        let current = candidates.len();
        if current <= target_depth {
            return 0;
        }
        candidates.sort_by_key(|t| t.created_at_secs);
        let to_remove = current - target_depth;
        let mut removed = 0usize;

        for tmpl in candidates.iter().take(to_remove) {
            let dir = self.template_dir(tmpl);
            match tokio::fs::remove_dir_all(&dir).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!(
                        "evict: failed to remove template dir {} ({}), keeping in pool",
                        tmpl.id,
                        e
                    );
                    continue;
                }
                Err(_) => {
                    log::info!(
                        "evict: template dir {} already gone, removing from pool",
                        tmpl.id
                    );
                }
                Ok(()) => {}
            }
            self.available
                .lock()
                .await
                .values_mut()
                .for_each(|q| q.retain(|t| t.id != tmpl.id));
            log::info!(
                "evict: removed template {} (snapshot_type={:?})",
                tmpl.id,
                kind
            );
            removed += 1;
        }

        removed
    }

    /// Delete a consumed template directory from disk.
    ///
    /// Called when the sandbox that consumed this template is deleted, so snapshot files
    /// (memory-ranges/, state.json, disk images) are reclaimed from disk.
    ///
    /// Only removes directories that have the `consumed` marker, so an available (not yet
    /// acquired) template is never accidentally deleted.
    pub async fn cleanup_consumed(&self, template_id: &str) {
        // find_template_dir searches environment/ then warmfork/, covering all SnapshotTypes.
        let dir = self.find_template_dir(template_id);
        let marker = dir.join(CONSUMED_MARKER);
        match tokio::fs::try_exists(&marker).await {
            Ok(true) => {}
            Ok(false) => {
                log::debug!(
                    "template {}: cleanup_consumed called but no consumed marker at {} — skipping",
                    template_id,
                    marker.display()
                );
                return;
            }
            Err(e) => {
                log::warn!(
                    "template {}: cleanup_consumed: cannot stat consumed marker: {}",
                    template_id,
                    e
                );
                return;
            }
        }
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => log::info!(
                "template {}: GC removed consumed template directory {}",
                template_id,
                dir.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!(
                "template {}: GC failed to remove directory {}: {}",
                template_id,
                dir.display(),
                e
            ),
        }
    }

    /// Delete consumed-marker directories that are not referenced by any active sandbox.
    ///
    /// Called after sandbox recovery on restart.  A consumed-marker directory that no
    /// recovered sandbox claims as its `template_id` was orphaned by a crash that occurred
    /// between `sandbox.base_dir` deletion and `cleanup_consumed()` — it is safe to remove.
    pub async fn gc_orphaned_consumed(&self, active_template_ids: &HashSet<String>) {
        // Scan both subdirectories: environment/ and warmfork/.
        for subdir in &["environment", "warmfork"] {
            let kind_dir = self.store_dir.join(subdir);
            let mut entries = match tokio::fs::read_dir(&kind_dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!(
                        "template pool gc_orphaned_consumed: read {}: {}",
                        kind_dir.display(),
                        e
                    );
                    continue;
                }
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let dir = entry.path();
                let id = match dir.file_name().and_then(|n| n.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                if !tokio::fs::try_exists(dir.join(CONSUMED_MARKER))
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                if active_template_ids.contains(&id) {
                    continue;
                }
                log::info!(
                    "template pool: GC orphaned consumed template {} in {} (not referenced by any recovered sandbox)",
                    id, subdir
                );
                if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("template pool: GC orphaned consumed {}: {}", id, e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use temp_dir::TempDir;

    use super::*;

    fn make_key() -> TemplateKey {
        TemplateKey::from_vm_config(
            "/var/lib/kuasar/vmlinux.bin",
            "/var/lib/kuasar/rootfs.img",
            2,
            512,
            "",
            "virtio-blk",
        )
    }

    fn make_template(id: &str) -> PooledTemplate {
        PooledTemplate::new(
            id,
            make_key(),
            PathBuf::from(format!("/var/lib/kuasar/templates/{}/snapshot", id)),
            "/var/lib/kuasar/rootfs.img",
            "/var/lib/kuasar/vmlinux.bin",
            2,
            512,
            format!("/var/lib/kuasar/templates/{}/task.vsock", id),
            format!("/tmp/{}-task.log", id),
        )
    }

    #[tokio::test]
    async fn test_pooled_template_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let tmpl = make_template("tmpl-001");
        tmpl.save(dir.path()).await.unwrap();
        let loaded = PooledTemplate::load(dir.path()).await.unwrap();
        assert_eq!(loaded.id, tmpl.id);
        assert_eq!(loaded.key, tmpl.key);
        assert_eq!(loaded.vcpus, 2);
        assert_eq!(loaded.memory_mb, 512);
    }

    #[tokio::test]
    async fn test_pooled_template_load_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = PooledTemplate::load(dir.path()).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("pooled_template.json"), "got: {}", msg);
    }

    #[tokio::test]
    async fn test_pool_add_acquire_lifo() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );
        let key = make_key();

        let t1 = make_template("t1");
        let t2 = make_template("t2");
        pool.add(t1).await.unwrap();
        pool.add(t2).await.unwrap();

        // LIFO: t2 was added last, should be acquired first
        let acquired = pool.acquire(&key).await.unwrap();
        assert_eq!(acquired.id, "t2");
        assert_eq!(pool.depth(&key).await, 1);
    }

    #[tokio::test]
    async fn test_pool_eviction_on_overflow() {
        let dir = TempDir::new().unwrap();
        // Create subdirs so remove_dir_all has something to delete
        for name in &["t1", "t2", "t3", "t4"] {
            tokio::fs::create_dir_all(dir.path().join(name))
                .await
                .unwrap();
        }
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            2,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );
        let key = make_key();

        for id in &["t1", "t2", "t3"] {
            let mut tmpl = make_template(id);
            // point snapshot_dir into temp dir so eviction can find the dir
            tmpl.id = id.to_string();
            pool.add(tmpl).await.unwrap();
        }

        // max_per_key=2: t1 (oldest) should have been evicted
        assert_eq!(pool.depth(&key).await, 2);

        let a = pool.acquire(&key).await.unwrap();
        let b = pool.acquire(&key).await.unwrap();
        // LIFO: t3, then t2 (t1 evicted)
        assert_eq!(a.id, "t3");
        assert_eq!(b.id, "t2");
        assert!(pool.acquire(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_pool_release_puts_back() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );
        let key = make_key();

        pool.add(make_template("t1")).await.unwrap();
        let tmpl = pool.acquire(&key).await.unwrap();
        assert!(pool.acquire(&key).await.is_none());

        pool.release(tmpl).await;
        assert_eq!(pool.depth(&key).await, 1);
    }

    #[tokio::test]
    async fn test_template_lease_completes_environment_restore_pin() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let tmpl = make_template("lease-environment");
        pool.in_flight_restores
            .lock()
            .await
            .insert(tmpl.id.clone(), 1);

        TemplateLease::new(pool.clone(), tmpl).complete().await;

        assert!(pool.in_flight_restores.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_template_lease_failure_derefs_shared_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let tmpl = make_container_template("lease-shared");
        pool.in_use_count.lock().await.insert(tmpl.id.clone(), 1);

        TemplateLease::new(pool.clone(), tmpl).fail().await;

        assert!(pool.in_use_count.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_template_lease_failure_releases_exclusive_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("lease-exclusive");

        TemplateLease::new(pool.clone(), tmpl).fail().await;

        assert_eq!(pool.depth(&key).await, 1);
    }

    /// WarmFork + Shared pool: acquire_for_restore must peek (not consume).
    #[tokio::test]
    async fn test_acquire_for_restore_warmfork_shared_peeks_not_consumes() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("wf-shared");
        pool.available
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .push_back(tmpl);

        let acquired = pool.acquire_for_restore(&key).await;

        assert!(acquired.is_some());
        // Template must still be in pool (peeked, not consumed).
        assert_eq!(
            pool.depth(&key).await,
            1,
            "WarmFork Shared must not remove template from pool"
        );
        // in_use_count must have been incremented.
        let counts = pool.in_use_count.lock().await;
        assert_eq!(
            counts.len(),
            1,
            "WarmFork Shared must increment in_use_count"
        );
    }

    #[tokio::test]
    async fn test_rebuild_active_shared_refs_from_recovered_sandboxes() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );

        pool.in_use_count
            .lock()
            .await
            .insert("stale".to_string(), 7);
        pool.rebuild_active_refs(
            &[
                "tmpl-a".to_string(),
                "tmpl-a".to_string(),
                "tmpl-b".to_string(),
            ],
            &[],
        )
        .await;

        let counts = pool.in_use_count.lock().await;
        assert_eq!(counts.get("tmpl-a"), Some(&2));
        assert_eq!(counts.get("tmpl-b"), Some(&1));
        assert!(!counts.contains_key("stale"));
    }

    #[tokio::test]
    async fn test_pool_status_reports_active_refs_and_gc_blockers() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let shared = make_container_template("status-shared");
        let env = make_template("status-environment");
        pool.add(shared.clone()).await.unwrap();
        pool.add(env.clone()).await.unwrap();
        pool.in_use_count.lock().await.insert(shared.id.clone(), 2);
        pool.in_flight_restores
            .lock()
            .await
            .insert(env.id.clone(), 1);

        assert_eq!(
            pool.active_shared_refs().await,
            vec![(shared.id.clone(), 2)]
        );
        assert_eq!(
            pool.in_flight_restores_by_template().await,
            vec![(env.id.clone(), 1)]
        );
        assert_eq!(
            pool.gc_blocked_templates().await,
            vec![
                (env.id.clone(), "restore_in_flight".to_string()),
                (shared.id.clone(), "shared_refcount".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn test_acquire_for_restore_shared_pool_peeks_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("shared-pool");
        pool.add(tmpl.clone()).await.unwrap();

        let acquired = pool.acquire_for_restore(&key).await.unwrap();
        assert_eq!(acquired.id, tmpl.id);
        assert_eq!(pool.depth(&key).await, 1);
        assert_eq!(pool.in_use_count.lock().await.get(&tmpl.id), Some(&1));
        pool.deref(&tmpl.id).await;
    }

    #[tokio::test]
    async fn test_acquire_for_restore_exclusive_pool_consumes_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("exclusive-pool");
        pool.add(tmpl.clone()).await.unwrap();

        let acquired = pool.acquire_for_restore(&key).await.unwrap();
        assert_eq!(acquired.id, tmpl.id);
        assert_eq!(pool.depth(&key).await, 0);
        assert!(!pool.in_use_count.lock().await.contains_key(&tmpl.id));
    }

    /// Continuation key encodes generation: a different generation yields a different key.
    #[test]
    fn test_continuation_key_generation_prevents_stale_reuse() {
        let uid = "my-pod-uid";
        let key0 = TemplateKey::from_workload_identity(uid, 0);
        let key1 = TemplateKey::from_workload_identity(uid, 1);
        // Different generation → different pool key → no accidental reuse.
        assert_ne!(key0, key1, "generation must differentiate keys");
        assert_eq!(key0.key, "my-pod-uid:0");
        assert_eq!(key1.key, "my-pod-uid:1");
    }

    /// Continuation annotation constants must have the expected kuasar.io domain.
    #[test]
    fn test_continuation_annotation_constants() {
        assert_eq!(super::super::POD_UID_ANNOTATION, "kuasar.io/pod-uid");
        assert_eq!(
            super::super::WORKLOAD_GENERATION_ANNOTATION,
            "kuasar.io/workload-generation"
        );
    }

    #[tokio::test]
    async fn test_acquire_by_id_for_restore_pins_environment_template() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();

        let t1 = make_template("admin-environment-1");
        let t2 = make_template("admin-environment-2");
        pool.add(t1.clone()).await.unwrap();
        pool.add(t2).await.unwrap();

        let acquired = pool.acquire_by_id_for_restore(&t1.id).await.unwrap();
        assert_eq!(acquired.id, t1.id);
        assert_eq!(pool.depth(&key).await, 2);
        assert_eq!(pool.in_flight_restores.lock().await.get(&t1.id), Some(&1));
    }

    #[tokio::test]
    async fn test_acquire_by_id_for_restore_refs_shared_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("admin-shared");
        pool.add(tmpl.clone()).await.unwrap();

        let acquired = pool.acquire_by_id_for_restore(&tmpl.id).await.unwrap();
        assert_eq!(acquired.id, tmpl.id);
        assert_eq!(pool.depth(&key).await, 1);
        assert_eq!(pool.in_use_count.lock().await.get(&tmpl.id), Some(&1));
    }

    #[tokio::test]
    async fn test_acquire_by_id_for_restore_consumes_exclusive_container() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("admin-exclusive");
        pool.add(tmpl.clone()).await.unwrap();

        let acquired = pool.acquire_by_id_for_restore(&tmpl.id).await.unwrap();
        assert_eq!(acquired.id, tmpl.id);
        assert_eq!(pool.depth(&key).await, 0);
        assert!(
            tokio::fs::try_exists(pool.template_dir(&tmpl).join(CONSUMED_MARKER))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_evict_skips_active_shared_container_template() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("active-shared");
        pool.add(tmpl.clone()).await.unwrap();
        let _lease = pool.acquire_by_id_for_restore(&tmpl.id).await.unwrap();

        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(removed, 0);
        assert_eq!(pool.depth(&key).await, 1);

        pool.deref(&tmpl.id).await;
        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(removed, 1);
        assert_eq!(pool.depth(&key).await, 0);
    }

    #[tokio::test]
    async fn test_evict_skips_pinned_environment_template() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path().join("environment").join("pinned-bv"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("environment").join("idle-bv"))
            .await
            .unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let pinned = make_template("pinned-bv");
        let idle = make_template("idle-bv");
        pool.add(pinned.clone()).await.unwrap();
        pool.add(idle.clone()).await.unwrap();

        // Simulate a running sandbox that holds a non-Copy pin on `pinned`.
        pool.ref_template(&pinned.id).await;

        // gc target_depth=0 — should only remove the idle template.
        let removed = pool.evict_by_kind(&SnapshotType::Environment, 0).await;
        assert_eq!(removed, 1, "only the idle template should be evicted");
        assert_eq!(pool.depth(&key).await, 1, "pinned template must remain");

        // After deref the pinned template becomes eligible.
        pool.deref(&pinned.id).await;
        let removed = pool.evict_by_kind(&SnapshotType::Environment, 0).await;
        assert_eq!(removed, 1);
        assert_eq!(pool.depth(&key).await, 0);
    }

    #[tokio::test]
    async fn test_metrics_hit_rate() {
        let metrics = TemplateMetrics::default();
        assert_eq!(metrics.hit_rate(), 0.0);

        metrics.record_environment_hit(100);
        metrics.record_continuation_hit(200);
        metrics.record_miss();

        // 2 hits (1 environment + 1 continuation), 1 miss → total_starts=3
        let rate = metrics.hit_rate();
        assert!((rate - 2.0 / 3.0).abs() < 1e-9, "hit_rate got {}", rate);

        let env_rate = metrics.environment_hit_rate();
        assert!(
            (env_rate - 1.0 / 3.0).abs() < 1e-9,
            "environment_hit_rate got {}",
            env_rate
        );

        let cont_rate = metrics.continuation_hit_rate();
        assert!(
            (cont_rate - 1.0 / 3.0).abs() < 1e-9,
            "continuation_hit_rate got {}",
            cont_rate
        );

        // rates must sum to 1.0
        let miss_rate = 1.0 - rate;
        assert!((env_rate + cont_rate + miss_rate - 1.0).abs() < 1e-9);

        let avg = metrics.avg_restore_ms();
        assert!((avg - 150.0).abs() < 1e-9, "avg_restore_ms got {}", avg);

        assert!((metrics.environment_avg_restore_ms() - 100.0).abs() < 1e-9);
        assert!((metrics.continuation_avg_restore_ms() - 200.0).abs() < 1e-9);
    }

    fn make_container_template(id: &str) -> PooledTemplate {
        let mut tmpl = make_template(id);
        tmpl.snapshot_type = SnapshotType::WarmFork;
        tmpl
    }

    #[test]
    fn test_snapshot_type_serde_roundtrip() {
        for (st, expected_json) in [
            (SnapshotType::Environment, r#""environment""#),
            (SnapshotType::WarmFork, r#""warm_fork""#),
            (SnapshotType::Continuation, r#""continuation""#),
        ] {
            let json = serde_json::to_string(&st).unwrap();
            assert_eq!(json, expected_json, "serialize {:?}", st);
            let roundtrip: SnapshotType = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtrip, st, "roundtrip {:?}", st);
        }
    }

    #[test]
    fn test_snapshot_type_annotation_parsing() {
        assert_eq!(
            SnapshotType::from_annotation("warm-fork").unwrap(),
            SnapshotType::WarmFork
        );
        assert_eq!(
            SnapshotType::from_annotation("warm_fork").unwrap(),
            SnapshotType::WarmFork
        );
        assert_eq!(
            SnapshotType::from_annotation("continuation").unwrap(),
            SnapshotType::Continuation
        );
        assert!(SnapshotType::from_annotation("environment").is_err());
        assert!(SnapshotType::from_annotation("bogus").is_err());
    }

    #[test]
    fn test_template_key_from_workload_identity() {
        let key = TemplateKey::from_workload_identity("pod-uid-abc", 3);
        assert_eq!(key.key, "pod-uid-abc:3");
    }

    #[test]
    fn test_workload_identity_serde_roundtrip() {
        let id = WorkloadIdentity {
            pod_uid: "my-pod-uid".to_string(),
            generation: 2,
        };
        let json = serde_json::to_string(&id).unwrap();
        let back: WorkloadIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pod_uid, id.pod_uid);
        assert_eq!(back.generation, id.generation);
    }

    #[tokio::test]
    async fn test_pool_warmfork_max_per_key() {
        let dir = TempDir::new().unwrap();
        for name in &["ct1", "ct2", "ct3"] {
            tokio::fs::create_dir_all(dir.path().join(name))
                .await
                .unwrap();
        }
        // warmfork_max_per_key=2; environment limit is large so it doesn't interfere
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            2,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();

        for id in &["ct1", "ct2", "ct3"] {
            let mut tmpl = make_container_template(id);
            tmpl.id = id.to_string();
            pool.add(tmpl).await.unwrap();
        }

        // warmfork_max_per_key=2: ct1 (oldest) should be evicted
        assert_eq!(pool.depth(&key).await, 2);

        let a = pool.acquire(&key).await.unwrap();
        let b = pool.acquire(&key).await.unwrap();
        // LIFO: ct3, then ct2
        assert_eq!(a.id, "ct3");
        assert_eq!(b.id, "ct2");
        assert!(pool.acquire(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_load_from_disk_rehydrates() {
        let dir = TempDir::new().unwrap();
        {
            let pool = TemplatePool::new(
                dir.path().to_path_buf(),
                5,
                usize::MAX,
                usize::MAX,
                crate::sandbox::TemplateLeaseMode::default(),
                false,
            );
            pool.add(make_template("t1")).await.unwrap();
            pool.add(make_template("t2")).await.unwrap();
        }
        // Reload from disk
        let pool2 = TemplatePool::load_from_disk(
            dir.path().to_path_buf(),
            5,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(pool2.total_depth().await, 2);
    }

    /// Verify that when N tasks concurrently race to acquire the single template in the pool,
    /// exactly one wins and all others get None — the Mutex prevents double-acquisition.
    #[tokio::test]
    async fn test_concurrent_acquire_exactly_one_wins() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );

        let t = make_template("race-t1");
        // The dir must exist so acquire() can write the consumed marker.
        tokio::fs::create_dir_all(pool.template_dir(&t))
            .await
            .unwrap();
        pool.add(t).await.unwrap();

        let key = make_key();
        let mut handles = vec![];
        for _ in 0..8 {
            let pool = Arc::clone(&pool);
            let key = key.clone();
            handles.push(tokio::spawn(async move { pool.acquire(&key).await }));
        }

        let mut wins = 0usize;
        for h in handles {
            if h.await.unwrap().is_some() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one acquire should win the race");
        assert_eq!(pool.depth(&key).await, 0);
    }

    /// Verify that load_from_disk skips templates whose `consumed` marker was written before
    /// a crash, so an interrupted restore does not re-enter the pool after restart.
    #[tokio::test]
    async fn test_load_from_disk_skips_consumed_template() {
        let dir = TempDir::new().unwrap();

        // Write a template to disk and mark it consumed (simulates crash mid-restore).
        let crashed = make_template("crashed-t");
        let crashed_dir = dir.path().join("environment").join("crashed-t");
        tokio::fs::create_dir_all(&crashed_dir).await.unwrap();
        crashed.save(&crashed_dir).await.unwrap();
        tokio::fs::write(crashed_dir.join(CONSUMED_MARKER), b"")
            .await
            .unwrap();

        // Write a second template with no consumed marker — should load normally.
        let healthy = make_template("healthy-t");
        let healthy_dir = dir.path().join("environment").join("healthy-t");
        tokio::fs::create_dir_all(&healthy_dir).await.unwrap();
        healthy.save(&healthy_dir).await.unwrap();

        let pool = TemplatePool::load_from_disk(
            dir.path().to_path_buf(),
            5,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            pool.total_depth().await,
            1,
            "consumed template must be skipped"
        );
        let acquired = pool.acquire(&make_key()).await;
        assert!(acquired.is_some());
        assert_eq!(acquired.unwrap().id, "healthy-t");
    }

    /// Verify that release() removes the consumed marker and returns the template to the pool,
    /// so a failed restore does not permanently drain the pool.
    #[tokio::test]
    async fn test_release_after_acquire_restores_pool_depth() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            3,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );

        let t = make_template("rel-t1");
        tokio::fs::create_dir_all(pool.template_dir(&t))
            .await
            .unwrap();
        pool.add(t).await.unwrap();

        let key = make_key();
        let tmpl = pool.acquire(&key).await.unwrap();

        // After acquire the consumed marker exists and depth is 0.
        let marker = pool.template_dir(&tmpl).join(CONSUMED_MARKER);
        assert!(
            tokio::fs::try_exists(&marker).await.unwrap(),
            "consumed marker must be written by acquire"
        );
        assert_eq!(pool.depth(&key).await, 0);

        // After release the marker is gone and the template is back.
        pool.release(tmpl).await;
        assert!(
            !tokio::fs::try_exists(&marker).await.unwrap(),
            "consumed marker must be removed by release"
        );
        assert_eq!(pool.depth(&key).await, 1);
    }

    /// Stress-test concurrent evict_by_kind and acquire calls to verify
    /// pool accounting stays consistent (no double-free, no phantom entries).
    #[tokio::test]
    async fn test_concurrent_gc_and_acquire_stays_consistent() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            10,
            usize::MAX,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::default(),
            false,
        );

        // Add 5 templates with real on-disk dirs.
        for i in 0..5usize {
            let id = format!("stress-{}", i);
            let t = make_template(&id);
            tokio::fs::create_dir_all(pool.template_dir(&t))
                .await
                .unwrap();
            pool.add(t).await.unwrap();
        }
        let initial = pool.total_depth().await;
        assert_eq!(initial, 5);

        let key = make_key();
        // Spawn 4 acquirers and 2 gc tasks simultaneously.
        let mut handles = vec![];
        for _ in 0..4 {
            let pool = Arc::clone(&pool);
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                let _ = pool.acquire(&key).await;
            }));
        }
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            handles.push(tokio::spawn(async move {
                pool.evict_by_kind(&SnapshotType::Environment, 1).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Pool depth must be ≤ initial and must not have gone negative or wrapped.
        let final_depth = pool.total_depth().await;
        assert!(
            final_depth <= initial,
            "pool depth {} exceeded initial {}",
            final_depth,
            initial
        );
    }

    // -------------------------------------------------------------------------
    // Fault injection / restart recovery tests
    // -------------------------------------------------------------------------

    /// WarmFork+Exclusive: after a restore fails (lease.fail → release), the template
    /// is returned to the pool and depth recovers to 1.
    /// This mirrors what happens when inject_task times out or returns an error.
    #[tokio::test]
    async fn test_warmfork_exclusive_inject_fail_releases_template() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("wf-excl-inject-fail");
        tokio::fs::create_dir_all(pool.template_dir(&tmpl))
            .await
            .unwrap();
        pool.available
            .lock()
            .await
            .entry(key.clone())
            .or_default()
            .push_back(tmpl.clone());

        // Simulate acquire + failed restore.
        let acquired = pool.acquire_for_restore(&key).await.unwrap();
        assert_eq!(pool.depth(&key).await, 0, "template consumed on acquire");

        // inject_task fails → lease.fail() → release()
        let lease = TemplateLease::new(pool.clone(), acquired);
        lease.fail().await;

        assert_eq!(
            pool.depth(&key).await,
            1,
            "template must be returned after inject failure"
        );
        // Consumed marker must have been removed so crash-recovery can reload the template.
        assert!(
            !tokio::fs::try_exists(
                pool.template_dir(&make_container_template("wf-excl-inject-fail"))
                    .join(CONSUMED_MARKER)
            )
            .await
            .unwrap(),
            "consumed marker must be removed after release"
        );
    }

    /// WarmFork+Shared: after a restore fails (lease.fail → deref), in_use_count drops
    /// to 0 and the template becomes GC-eligible.
    /// Mirrors inject_task timeout in a Shared pool.
    #[tokio::test]
    async fn test_warmfork_shared_inject_fail_derefs_and_enables_gc() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("wf-shared-inject-fail");
        pool.add(tmpl.clone()).await.unwrap();

        // acquire_for_restore peeks and increments in_use_count.
        let acquired = pool.acquire_for_restore(&key).await.unwrap();
        assert_eq!(
            *pool.in_use_count.lock().await.get(&tmpl.id).unwrap_or(&0),
            1,
        );
        assert_eq!(pool.depth(&key).await, 1, "template stays in Shared pool");

        // inject_task fails → lease.fail() → deref.
        let lease = TemplateLease::new(pool.clone(), acquired);
        lease.fail().await;

        assert!(
            pool.in_use_count.lock().await.is_empty(),
            "in_use_count must reach 0 after inject failure"
        );
        // Template still in pool (not consumed); GC can now evict it.
        assert_eq!(pool.depth(&key).await, 1);
        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(
            removed, 1,
            "template must be GC-eligible after in_use_count=0"
        );
    }

    /// Restart recovery: gc_orphaned_consumed must delete a WarmFork+Exclusive
    /// consumed template when its sandbox is no longer active (crash after consume,
    /// before sandbox reached Running state).
    #[tokio::test]
    async fn test_gc_orphaned_consumed_deletes_stale_warmfork_exclusive() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        // Simulate an orphaned consumed template: directory + consumed marker exist,
        // but no sandbox references this template_id.
        let orphan_dir = dir.path().join("warmfork").join("orphan-wf");
        tokio::fs::create_dir_all(&orphan_dir).await.unwrap();
        tokio::fs::write(orphan_dir.join(CONSUMED_MARKER), b"")
            .await
            .unwrap();

        let active_ids = std::collections::HashSet::new(); // no active sandboxes
        pool.gc_orphaned_consumed(&active_ids).await;

        assert!(
            !tokio::fs::try_exists(&orphan_dir).await.unwrap(),
            "orphaned consumed template directory must be deleted"
        );
    }

    /// Restart recovery: gc_orphaned_consumed must keep a WarmFork+Exclusive
    /// consumed template when a running sandbox still references it.
    #[tokio::test]
    async fn test_gc_orphaned_consumed_keeps_active_warmfork_exclusive() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Exclusive,
            false,
        );
        let active_dir = dir.path().join("warmfork").join("active-wf");
        tokio::fs::create_dir_all(&active_dir).await.unwrap();
        tokio::fs::write(active_dir.join(CONSUMED_MARKER), b"")
            .await
            .unwrap();

        let mut active_ids = std::collections::HashSet::new();
        active_ids.insert("active-wf".to_string()); // sandbox still running
        pool.gc_orphaned_consumed(&active_ids).await;

        assert!(
            tokio::fs::try_exists(&active_dir).await.unwrap(),
            "consumed template directory must be kept while sandbox is active"
        );
        assert!(
            tokio::fs::try_exists(active_dir.join(CONSUMED_MARKER))
                .await
                .unwrap(),
            "consumed marker must be preserved"
        );
    }

    /// Full restart simulation for WarmFork+Shared:
    /// rebuild_active_refs restores in_use_count → GC still blocked →
    /// after simulated sandbox delete (deref) → GC proceeds.
    #[tokio::test]
    async fn test_restart_warmfork_shared_refcount_recovery_then_gc() {
        let dir = TempDir::new().unwrap();
        let pool = TemplatePool::new(
            dir.path().to_path_buf(),
            usize::MAX,
            3,
            usize::MAX,
            crate::sandbox::TemplateLeaseMode::Shared,
            false,
        );
        let key = make_key();
        let tmpl = make_container_template("wf-restart");
        pool.add(tmpl.clone()).await.unwrap();

        // Pre-restart: two sandboxes were using this template.
        // After restart in_use_count is empty; rebuild it.
        assert!(pool.in_use_count.lock().await.is_empty());
        pool.rebuild_active_refs(&[tmpl.id.clone(), tmpl.id.clone()], &[])
            .await;

        let counts = pool.in_use_count.lock().await;
        assert_eq!(
            counts.get(&tmpl.id),
            Some(&2),
            "two refs rebuilt after restart"
        );
        drop(counts);

        // GC must not evict the template while in_use_count > 0.
        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(removed, 0, "GC must be blocked with in_use_count=2");

        // First sandbox deleted.
        pool.deref(&tmpl.id).await;
        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(removed, 0, "GC still blocked with in_use_count=1");

        // Second sandbox deleted → in_use_count=0 → GC eligible.
        pool.deref(&tmpl.id).await;
        assert!(pool.in_use_count.lock().await.is_empty());
        let removed = pool.evict_by_kind(&SnapshotType::WarmFork, 0).await;
        assert_eq!(removed, 1, "GC must proceed after all sandboxes deleted");
        assert_eq!(pool.depth(&key).await, 0);
    }
}
