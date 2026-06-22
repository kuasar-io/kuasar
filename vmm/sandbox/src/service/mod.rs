pub mod admin;
pub mod grpc;
pub mod sandbox;
pub mod snapshot;

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::{Mutex, RwLock};

use crate::{
    sandbox::{KuasarSandbox, SnapshotConfig},
    template::{ContinuationStore, TemplatePool},
    vm::{Snapshottable, VMFactory, VM},
};

/// Shared state extracted from `KuasarSandboxer` for the admin and gRPC service servers.
/// All fields are cheaply cloneable via `Arc`.
pub struct Handle<F>
where
    F: VMFactory + Sync + Send + 'static,
    F::VM: VM + Snapshottable + Sync + Send + 'static,
{
    pub factory: Arc<F>,
    #[allow(clippy::type_complexity)]
    pub sandboxes: Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
    pub pool: Option<Arc<TemplatePool>>,
    pub continuation_store: Option<Arc<ContinuationStore>>,
    pub snapshot_config: SnapshotConfig,
    /// Root directory for sandbox slots created by the gRPC service.
    /// Set to `<sandboxer --dir>/`.
    pub sandbox_base_dir: PathBuf,
    /// Reverse index: pod_uid → sandbox_id.
    /// Rebuilt from persisted state on startup; kept up-to-date at create/delete time.
    pub pod_uid_index: Arc<RwLock<HashMap<String, String>>>,
}
