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

pub mod continuation;
pub use continuation::{ContinuationLease, ContinuationStore};

mod metrics;
mod pool;
mod types;

pub use metrics::TemplateMetrics;
pub(crate) use pool::TemplateLease;
pub use pool::TemplatePool;
pub use types::{
    CreateTemplateRequest, PooledTemplate, SnapshotContainerMeta, SnapshotType, TemplateKey,
    WorkloadIdentity,
};

/// Pod annotation that carries a user-defined template pool key.
///
/// - **VM snapshots** (bare VM): key is auto-computed from VM config; do not set this annotation.
/// - **WarmFork snapshots**: set this annotation so sandboxes with the same workload can share
///   a pre-warmed snapshot.  The value is opaque to the sandboxer — correctness is the caller's
///   responsibility.
/// - **Continuation snapshots**: use `POD_UID_ANNOTATION` and `WORKLOAD_GENERATION_ANNOTATION`
///   instead; the pool key is derived automatically from workload identity.
/// - **Restore pods**: may also set `SNAPSHOT_TYPE_ANNOTATION` to explicitly select
///   `warm-fork` or `continuation`. `environment` is the default when this annotation is absent.
pub const SNAPSHOT_TYPE_ANNOTATION: &str = "kuasar.io/snapshot-type";
pub const TEMPLATE_KEY_ANNOTATION: &str = "kuasar.io/template-key";
/// Pin a WarmFork restore to a specific template by ID instead of picking the latest
/// for `TEMPLATE_KEY_ANNOTATION`. When both annotations are set, both must match.
pub const TEMPLATE_ID_ANNOTATION: &str = "kuasar.io/template-id";

/// Pod annotation for Continuation restore: the original Pod UID.
///
/// Combined with `WORKLOAD_GENERATION_ANNOTATION` to form the workload identity key
/// (`{pod_uid}:{generation}`) that uniquely identifies the continuation snapshot.
/// Required for Continuation restore; ignored for other snapshot types.
pub const POD_UID_ANNOTATION: &str = "kuasar.io/pod-uid";

/// Pod annotation for Continuation restore: the workload generation.
///
/// Must match the `generation` used when creating the template via the admin API.
/// Defaults to `"0"` if absent.
pub const WORKLOAD_GENERATION_ANNOTATION: &str = "kuasar.io/workload-generation";

/// Generate a server-owned template ID.
///
/// The ID is intentionally type-agnostic; snapshot type and workload identity live in metadata
/// and, for Continuation, in the store path.
pub fn new_template_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
