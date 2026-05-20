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

use std::{fmt, path::PathBuf};

use anyhow::anyhow;
use containerd_sandbox::error::Error;
use log::warn;

use crate::vm::VM;

/// Tracks the current phase of a restore operation for structured error messages and rollback.
///
/// # Design note: intentional deviation from the proposal
///
/// The `snapshot_restore.md` proposal defines `RestoreTransaction` as a unified struct that
/// owns `TemplateLease`, `PreparedMemory`, and `PreparedBlock` alongside the phase.  This
/// implementation separates those concerns:
///
/// - `TemplateLease` is owned by `start_with_template()` (the Sandboxer layer) and its
///   rollback is called there on failure.
/// - `PreparedMemory` and `PreparedBlock` are ephemeral values passed through `RestoreSource`
///   and `vm.restore()`; they are cleaned up implicitly by `vm.stop()` on rollback.
///
/// `RestoreTransaction` here is only a phase tracker and error formatter.  The rollback paths
/// are scattered across the call stack rather than centralised in the transaction object.
/// If rollback logic grows more complex (e.g. external UFFD handler teardown, block device
/// detach sequencing), revisit this and consolidate into the proposal's unified model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RestorePhase {
    RestoreVm,
    InitClient,
    /// Environment + WarmFork: hotplug tap devices into restored VM.
    HotplugNetwork,
    /// WarmFork only: inject task parameters via guest agent task-injection API.
    InjectTask,
    /// Environment only: push sandbox config files (hostname, resolv.conf, hosts).
    PushSandboxFiles,
    /// Environment only: call setup_sandbox() to initialize guest namespaces.
    SetupSandbox,
    /// Continuation: first restore step; ContinuationNetworkController (not yet implemented)
    /// will route or attach the original virtual IP / Pod IP to this node.
    TransferNetworkIdentity,
    Commit,
}

impl fmt::Display for RestorePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RestoreVm => write!(f, "restore_vm"),
            Self::InitClient => write!(f, "init_client"),
            Self::HotplugNetwork => write!(f, "hotplug_network"),
            Self::InjectTask => write!(f, "inject_task"),
            Self::PushSandboxFiles => write!(f, "push_sandbox_files"),
            Self::SetupSandbox => write!(f, "setup_sandbox"),
            Self::TransferNetworkIdentity => write!(f, "transfer_network_identity"),
            Self::Commit => write!(f, "commit"),
        }
    }
}

pub(crate) struct RestoreTransaction {
    sandbox_id: String,
    work_dir: PathBuf,
    phase: RestorePhase,
}

impl RestoreTransaction {
    pub(crate) fn new(sandbox_id: impl Into<String>, work_dir: PathBuf) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            work_dir,
            phase: RestorePhase::RestoreVm,
        }
    }

    pub(crate) fn set_phase(&mut self, phase: RestorePhase) {
        self.phase = phase;
    }

    pub(crate) fn phase(&self) -> RestorePhase {
        self.phase
    }

    pub(crate) fn fail<E: fmt::Display>(&self, err: E) -> Error {
        anyhow!(
            "restore sandbox {} failed at phase {}: {}",
            self.sandbox_id,
            self.phase,
            err
        )
        .into()
    }

    pub(crate) async fn rollback_vm<V>(&self, vm: &mut V)
    where
        V: VM + Sync + Send,
    {
        if let Err(e) = vm.stop(true).await {
            warn!(
                "sandbox {}: rollback VM during restore phase {}: {}",
                self.sandbox_id, self.phase, e
            );
        }
    }

    pub(crate) async fn cleanup_work_dir(&self) {
        if let Err(e) = tokio::fs::remove_dir_all(&self.work_dir).await {
            warn!(
                "sandbox {}: cleanup restore work_dir {} after phase {}: {}",
                self.sandbox_id,
                self.work_dir.display(),
                self.phase,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restore_phase_display() {
        assert_eq!(RestorePhase::RestoreVm.to_string(), "restore_vm");
        assert_eq!(RestorePhase::HotplugNetwork.to_string(), "hotplug_network");
    }

    #[test]
    fn test_restore_transaction_error_contains_phase() {
        let mut txn = RestoreTransaction::new("sandbox-a", PathBuf::from("/tmp/restore"));
        txn.set_phase(RestorePhase::SetupSandbox);

        let err = txn.fail("boom");
        let msg = err.to_string();
        assert!(msg.contains("sandbox-a"));
        assert!(msg.contains("setup_sandbox"));
        assert!(msg.contains("boom"));
    }
}
