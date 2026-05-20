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

use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic counters for pool health monitoring.
#[derive(Default)]
pub struct TemplateMetrics {
    pub templates_created: AtomicU64,
    /// Total sandbox start attempts that went through the pool path (hits + misses).
    pub total_starts: AtomicU64,
    pub pool_hits: AtomicU64,
    pub pool_misses: AtomicU64,
    /// Cumulative restore latency across all kinds for computing the overall average.
    pub restore_total_ms: AtomicU64,
    pub restore_count: AtomicU64,
    // Per-kind hit counters and latency accumulators.
    pub environment_hits: AtomicU64,
    pub warmfork_hits: AtomicU64,
    /// Continuation hits (always 0 until Continuation restore is implemented).
    pub continuation_hits: AtomicU64,
    pub environment_restore_ms: AtomicU64,
    pub warmfork_restore_ms: AtomicU64,
    pub continuation_restore_ms: AtomicU64,
    pub environment_restore_count: AtomicU64,
    pub warmfork_restore_count: AtomicU64,
    pub continuation_restore_count: AtomicU64,
}

impl TemplateMetrics {
    pub fn record_environment_hit(&self, restore_ms: u64) {
        self.total_starts.fetch_add(1, Ordering::Relaxed);
        self.pool_hits.fetch_add(1, Ordering::Relaxed);
        self.restore_total_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.restore_count.fetch_add(1, Ordering::Relaxed);
        self.environment_hits.fetch_add(1, Ordering::Relaxed);
        self.environment_restore_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.environment_restore_count
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_warmfork_hit(&self, restore_ms: u64) {
        self.total_starts.fetch_add(1, Ordering::Relaxed);
        self.pool_hits.fetch_add(1, Ordering::Relaxed);
        self.restore_total_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.restore_count.fetch_add(1, Ordering::Relaxed);
        self.warmfork_hits.fetch_add(1, Ordering::Relaxed);
        self.warmfork_restore_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.warmfork_restore_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_continuation_hit(&self, restore_ms: u64) {
        self.total_starts.fetch_add(1, Ordering::Relaxed);
        self.pool_hits.fetch_add(1, Ordering::Relaxed);
        self.restore_total_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.restore_count.fetch_add(1, Ordering::Relaxed);
        self.continuation_hits.fetch_add(1, Ordering::Relaxed);
        self.continuation_restore_ms
            .fetch_add(restore_ms, Ordering::Relaxed);
        self.continuation_restore_count
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.total_starts.fetch_add(1, Ordering::Relaxed);
        self.pool_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_template_created(&self) {
        self.templates_created.fetch_add(1, Ordering::Relaxed);
    }

    /// Overall pool hit rate as a value in [0.0, 1.0]. Returns 0.0 if no requests yet.
    pub fn hit_rate(&self) -> f64 {
        let hits = self.pool_hits.load(Ordering::Relaxed) as f64;
        let total = self.total_starts.load(Ordering::Relaxed) as f64;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }

    /// Fraction of all sandbox starts that hit an Environment snapshot.
    pub fn environment_hit_rate(&self) -> f64 {
        let hits = self.environment_hits.load(Ordering::Relaxed) as f64;
        let total = self.total_starts.load(Ordering::Relaxed) as f64;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }

    /// Fraction of all sandbox starts that hit a WarmFork snapshot.
    pub fn warmfork_hit_rate(&self) -> f64 {
        let hits = self.warmfork_hits.load(Ordering::Relaxed) as f64;
        let total = self.total_starts.load(Ordering::Relaxed) as f64;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }

    /// Fraction of all sandbox starts that hit a Continuation snapshot.
    pub fn continuation_hit_rate(&self) -> f64 {
        let hits = self.continuation_hits.load(Ordering::Relaxed) as f64;
        let total = self.total_starts.load(Ordering::Relaxed) as f64;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }

    /// Average restore latency in milliseconds across all pool-hit restores.
    pub fn avg_restore_ms(&self) -> f64 {
        let total = self.restore_total_ms.load(Ordering::Relaxed) as f64;
        let count = self.restore_count.load(Ordering::Relaxed) as f64;
        if count == 0.0 {
            0.0
        } else {
            total / count
        }
    }

    pub fn environment_avg_restore_ms(&self) -> f64 {
        let total = self.environment_restore_ms.load(Ordering::Relaxed) as f64;
        let count = self.environment_restore_count.load(Ordering::Relaxed) as f64;
        if count == 0.0 {
            0.0
        } else {
            total / count
        }
    }

    pub fn warmfork_avg_restore_ms(&self) -> f64 {
        let total = self.warmfork_restore_ms.load(Ordering::Relaxed) as f64;
        let count = self.warmfork_restore_count.load(Ordering::Relaxed) as f64;
        if count == 0.0 {
            0.0
        } else {
            total / count
        }
    }

    pub fn continuation_avg_restore_ms(&self) -> f64 {
        let total = self.continuation_restore_ms.load(Ordering::Relaxed) as f64;
        let count = self.continuation_restore_count.load(Ordering::Relaxed) as f64;
        if count == 0.0 {
            0.0
        } else {
            total / count
        }
    }
}
