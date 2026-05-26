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

//! # WarmFork Injection Protocol — Specification
//!
//! This module is the **canonical specification** for the WarmFork injection protocol.
//! Rust message types and framing live in [`crate::warmfork::message`]
//! (re-exported from `vmm_common::warmfork::message`).
//! If this document and the code disagree, fix the code.
//!
//! ---
//!
//! ## Overview
//!
//! WarmFork allows a process that has completed expensive initialisation (model loading,
//! JVM warm-up, cache fill) to be snapshotted and then restored multiple times as
//! independent task instances with zero cold-start latency. After each restore, the
//! sandboxer delivers the task identity to the process through the **injection protocol**;
//! the process then begins execution.
//!
//! The protocol supports two topologies:
//!
//! - **Single-container**: one workload container in the Pod implements the injection
//!   protocol (default; `kuasar.io/warm-fork-containers` may be omitted for single-pod).
//! - **Multi-container**: multiple workload containers each implement the injection
//!   protocol; the sandboxer uses a two-phase pre-commit barrier to avoid partial startup
//!   before COMMIT.
//!
//! ---
//!
//! ## Protocol
//!
//! Version string: `"1"` (stored in [`PROTOCOL_V1`] and in `PooledTemplate.ready_protocol_version`).
//!
//! ### Transport
//!
//! - Unix domain socket, `SOCK_STREAM`.
//! - Default path: `/run/warmfork-readiness.sock`.
//! - Overridden per-pod via annotation `kuasar.io/warm-fork-readiness-socket`.
//! - Per-container override: `kuasar.io/container/<name>/warm-fork-readiness-socket`.
//! - The workload creates the socket; the sandboxer (via guest agent) connects to it.
//! - The guest agent enters the container's mount namespace via `setns` before connecting.
//!
//! ### Message framing
//!
//! All messages use a **4-byte big-endian length prefix** followed by a UTF-8 JSON body.
//! Maximum frame body: 4 MiB (`MAX_FRAME_LEN`).
//!
//! ### Phase sequence
//!
//! ```text
//! Workload (guest)                       Sandboxer (host, via guest agent)
//! ────────────────                       ─────────────────────────────────
//! bind(inject_socket)
//! listen(inject_socket)
//!   │
//!   │   ← pre-snapshot probe (CheckInjectSocket RPC) →
//! accept()                ◄────────────── connect(inject_socket)
//! send CAPABILITIES ──────────────────►  read CAPABILITIES; verify version; close
//! (EOF) → loop back to accept()
//!   │
//!   │                                    vm.restore() completes
//!   │                                    guest agent starts
//!   │                                    reseed_guest_entropy()   ← before inject
//!   │                                    InjectTask RPC fires
//! accept()                ◄────────────── connect(inject_socket)
//! send CAPABILITIES ──────────────────►  read CAPABILITIES; verify version
//!                         ◄────────────── PREPARE(task_id, env, context)
//! validate (no execution)
//! send READY ─────────────────────────►  [barrier: wait for all targets]
//!                         ◄────────────── COMMIT (after all targets READY)
//! send STARTED ───────────────────────►  restore committed, lease locked
//! close; begin task execution
//! ```
//!
//! On rollback, the sandboxer sends CANCEL instead of PREPARE (or after PREPARE
//! if another target REJECTs), then stops the VM.
//! CANCEL is never sent after COMMIT has been issued.
//!
//! ### Messages
//!
//! See [`crate::warmfork::message`] for the Rust types.  Summary:
//!
//! | Direction          | Type           | Description                                  |
//! |--------------------|----------------|----------------------------------------------|
//! | workload → sboxer  | `CAPABILITIES` | First message on every connection            |
//! | sboxer → workload  | `PREPARE`      | Phase-1: per-task parameters for validation  |
//! | workload → sboxer  | `READY`        | Validated; no side effects produced yet      |
//! | sboxer → workload  | `COMMIT`       | Phase-2: all targets READY; begin execution  |
//! | workload → sboxer  | `STARTED`      | Formal commit point; task begins after this  |
//! | workload → sboxer  | `REJECT`       | Validation failure; sandboxer rolls back     |
//! | sboxer → workload  | `CANCEL`       | Rollback signal; workload should clean up    |
//! | workload → sboxer  | `CANCEL_ACK`   | Optional acknowledgement of CANCEL           |
//!
//! ---
//!
//! ## Snapshot Quiescent Contract
//!
//! The WarmFork protocol makes **strong determinism guarantees** that only hold if the
//! workload process is in a **quiescent state** at snapshot time.
//!
//! A process is in the Snapshot Quiescent State when ALL of the following hold:
//!
//! 1. **No running worker threads** (or only safe-idle background threads with no
//!    external connections, no task-specific identity, and no non-idempotent state).
//! 2. **No active outbound network connections.**
//! 3. **No in-flight I/O** (`io_uring`, `epoll`, `aio_*`).
//! 4. **No armed non-idempotent timers.**
//! 5. **No pending signals.**
//! 6. **All mutable state committed** to persistent storage.
//! 7. **No process-level file locks** (`fcntl`/`flock`).
//!
//! The `kuasar.io/warm-fork-ready-protocol-version: 1` annotation is the workload's
//! machine-readable assertion that all conditions above are satisfied.
//! The sandboxer runs the `CheckInjectSocket` probe before `vm.snapshot()` but
//! cannot verify conditions 1–7 directly.
//!
//! ---

/// The only supported protocol version string.
/// Equals `vmm_common::warmfork::message::PROTOCOL_VERSION`.
/// Stored in `PooledTemplate.ready_protocol_version` and validated at both snapshot
/// creation time (in `from_sandbox`) and restore time (in `start_with_template`).
pub const PROTOCOL_V1: &str = "1";

/// Annotation key on the pod that declares the workload's protocol readiness.
/// Value must equal [`PROTOCOL_V1`] (`"1"`).  This is the workload's machine-readable
/// assertion that it has been authored to comply with the Snapshot Quiescent Contract above.
pub const READY_PROTOCOL_ANNOTATION: &str = "kuasar.io/warm-fork-ready-protocol-version";

// ── Readiness socket path annotations ────────────────────────────────────────

/// Annotation key for the per-pod readiness socket path override (single-container mode).
/// When absent, the guest agent uses `/run/warmfork-readiness.sock`.
pub const READINESS_SOCKET_ANNOTATION: &str = "kuasar.io/warm-fork-readiness-socket";

/// Prefix for per-container readiness socket path overrides.
/// Full key: `kuasar.io/container/<name>/warm-fork-readiness-socket`.
/// Takes priority over the pod-level [`READINESS_SOCKET_ANNOTATION`].
pub const PER_CONTAINER_READINESS_SOCKET_PREFIX: &str = "kuasar.io/container/";

/// Suffix appended after `<name>` for per-container readiness socket overrides.
pub const PER_CONTAINER_READINESS_SOCKET_SUFFIX: &str = "/warm-fork-readiness-socket";

// ── Multi-container annotation ────────────────────────────────────────────────

/// Annotation key listing the workload container names that participate in injection.
///
/// Value: comma-separated pod-spec container names.
/// - Absent with a single-container pod: single-container mode.
/// - Absent with a multi-container pod: **error** — must be declared explicitly.
/// - Parsing rules: trim whitespace; reject empty entries, duplicates, and names not
///   present in the pod spec.
pub const WARM_FORK_CONTAINERS_ANNOTATION: &str = "kuasar.io/warm-fork-containers";

// ── Task-parameter annotations (restore time) ─────────────────────────────────

/// Annotation key carrying the per-restore task identifier.
/// Must be non-empty.  Absent or empty → the sandboxer rejects the restore before
/// starting the VM. Pod-level only; cannot be overridden per-container.
pub const TASK_ID_ANNOTATION: &str = "kuasar.io/task-id";

/// Annotation key carrying opaque per-restore context (e.g. serialized request).
/// Pod-level fallback; per-container override: `kuasar.io/container/<name>/task-context`.
pub const TASK_CONTEXT_ANNOTATION: &str = "kuasar.io/task-context";

/// Annotation key prefix for pod-level per-restore environment overrides.
/// Full key: `kuasar.io/task-env/<ENV_VAR_NAME>`.
/// Per-container override prefix: `kuasar.io/container/<name>/task-env/`.
pub const TASK_ENV_PREFIX: &str = "kuasar.io/task-env/";

/// Prefix for per-container task-context annotation.
/// Full key: `kuasar.io/container/<name>/task-context`.
pub const PER_CONTAINER_TASK_CONTEXT_PREFIX: &str = "kuasar.io/container/";

/// Suffix appended after `<name>` for per-container task-context overrides.
pub const PER_CONTAINER_TASK_CONTEXT_SUFFIX: &str = "/task-context";

/// Prefix for per-container task-env annotation.
/// Full key: `kuasar.io/container/<name>/task-env/<ENV_VAR_NAME>`.
pub const PER_CONTAINER_TASK_ENV_PREFIX: &str = "kuasar.io/container/";

/// Infix in per-container task-env annotation key (between `<name>` and `<ENV_VAR_NAME>`).
pub const PER_CONTAINER_TASK_ENV_INFIX: &str = "/task-env/";

/// Default readiness socket path used when no annotation override is present.
pub const DEFAULT_READINESS_SOCKET: &str = "/run/warmfork-readiness.sock";

/// Protocol state machine for documentation purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolState {
    /// Workload has opened the readiness socket and is blocked on `accept()`.
    /// The VM is safe to snapshot at this point (if the Quiescent Contract holds).
    Ready,
    /// Sandboxer has connected; workload sent CAPABILITIES; awaiting PREPARE or EOF.
    Probed,
    /// Workload received PREPARE; is validating; no external side effects yet.
    Prepared,
    /// Workload sent READY; blocking for COMMIT or CANCEL.
    /// No external side effects have been produced.
    Holding,
    /// Workload received COMMIT; sends STARTED before any execution.
    Committing,
    /// STARTED sent; connection closed; workload begins task execution.
    /// Sandboxer has locked the restore lease.
    Started,
    /// Sandboxer sent CANCEL; workload cleaning up; VM will be stopped.
    Cancelled,
}

/// Reasons a protocol handshake can fail on the sandboxer side.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// Pod annotation `kuasar.io/warm-fork-ready-protocol-version` is missing or has wrong value.
    MissingOrInvalidReadyAnnotation { found: String },
    /// Inject socket file does not exist or is not in the kernel unix socket table.
    SocketNotListening { path: String },
    /// Task ID annotation is absent or empty.
    MissingTaskId,
    /// Phase-1 injection timed out (READY never received within `prepare_timeout_ms`).
    PrepareTimeout,
    /// Phase-2 injection timed out (STARTED never received within `commit_timeout_ms`).
    CommitTimeout,
    /// A target container was not found by the guest agent.
    TargetNotFound { container_id: String },
    /// setns into the container's mount namespace failed.
    NamespaceError { container_id: String },
    /// A workload target sent REJECT during the PREPARE phase.
    WorkloadRejected {
        container_id: String,
        reason: String,
    },
}
