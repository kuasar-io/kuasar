# SnapStart Compatibility Layer Design

<!-- toc -->
- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Background](#background)
  - [Existing Service API Model](#existing-service-api-model)
  - [Snapshot/Restore API: gRPC Services](#snapshotrestore-api-grpc-services)
  - [Orchestrator Snapshot Semantics](#orchestrator-snapshot-semantics)
  - [The Two Problems](#the-two-problems)
  - [Why gRPC for the Northbound API](#why-grpc-for-the-northbound-api)

- [Design Part 1: Snapshot Creation — SandboxSnapshotController gRPC Service](#design-part-1-snapshot-creation--sandboxsnapshotcontroller-grpc-service)
  - [Proto Definition](#proto-definition)
  - [Two-Service Split: Rationale](#two-service-split-rationale)
  - [CreateSandboxSnapshot Semantics](#createsandboxsnapshot-semantics)
  - [Handle Changes](#handle-changes)
- [Design Part 2: Restore — Two Paths](#design-part-2-restore--two-paths)
  - [Pause/Resume Path: SandboxController.PauseSandbox / ResumeSandbox](#pauseresume-path-sandboxcontrollerpausesandbox--resumesandbox)
  - [CRI-triggered Path: KuasarNativeResolver (containerd)](#cri-triggered-path-kuasarnativeresolver-containerd)
  - [Shared Restore Functions](#shared-restore-functions)
  - [Modified start() Flow](#modified-start-flow)
- [Design Part 3: Pause/Resume — VM Lifecycle State](#design-part-3-pauseresume--vm-lifecycle-state)
  - [Pause Semantics](#pause-semantics)
  - [Resume Semantics](#resume-semantics)
  - [containerd Status Reporting](#containerd-status-reporting)
  - [Guest Task Agent: Non-blocking Event Dispatch](#guest-task-agent-non-blocking-event-dispatch)
- [End-to-End Flows](#end-to-end-flows)
  - [WarmFork Snapshot Creation](#warmfork-snapshot-creation)
  - [WarmFork Restore — CRI Path](#warmfork-restore--cri-path)
  - [Continuation Snapshot Creation](#continuation-snapshot-creation)
  - [Continuation Restore — CRI Path](#continuation-restore--cri-path)
  - [Pause](#pause)
  - [Resume](#resume)
- [Responsibility Boundaries](#responsibility-boundaries)
- [Key Design Decisions](#key-design-decisions)
<!-- /toc -->

---

## Summary

This proposal addresses two distinct problems that arise when integrating upper-layer orchestrators (such as AgentCube) with Kuasar's snapshot and restore system:

1. **Snapshot creation**: The upper-layer orchestrator needs to create snapshots and reference them by a `snapshot_name` — a stable, caller-chosen name that maps directly to the internal template artifact, in contrast to Kuasar's auto-generated `template_id`. This is solved by introducing a `SandboxSnapshotController` gRPC service that exposes `CreateSandboxSnapshot`, `DeleteSandboxSnapshot`, and `ListSandboxSnapshots`.

2. **Restore path**: Restore can be triggered by two fundamentally different callers with incompatible integration models. Upper-layer orchestrators manage sandbox lifecycles directly via Kuasar's northbound API, bypassing kubelet entirely; they need a synchronous gRPC call to pause and later resume an existing sandbox. containerd, on the other hand, invokes Kuasar's sandboxer through the `RunPodSandbox` sandbox plugin call, transparently passing pod annotations from the Pod spec. Both callers must be supported without either path interfering with the other. To address this, two restore paths are defined: one path is a `SandboxController.PauseSandbox` / `ResumeSandbox` gRPC call issued directly by the upper-layer orchestrator; another path introduces `KuasarNativeResolver` to handle containerd-triggered restores via the CRI path.

---

## Motivation

Kuasar's snapshot capabilities — WarmFork (`TemplatePool`) and Continuation (`ContinuationStore`) — are exposed only through two internal mechanisms: a JSON admin socket for management operations and `kuasar.io/*` CRI annotations for restore triggers. Neither interface gives the orchestration layer a stable, discoverable abstraction over Kuasar's SnapStart capabilities.

This proposal introduces a compatibility layer that serves as that abstraction. Through plugin introspection interfaces (`GetPluginInfo`, `GetPluginCapabilities`, `Probe`), the orchestration layer can discover at runtime which snapshot modes a given Kuasar node supports — WarmFork, Continuation, or both — and verify liveness before scheduling snapshot-dependent workloads. This capability discovery is the foundation that makes the rest of the API composable: an orchestrator queries capabilities, selects a mode, creates a snapshot, and later creates sandboxes from it, all through a uniform gRPC interface without coupling to Kuasar's internal structures.

Concretely, the orchestration layer needs to:

- (a) Create snapshots on Kuasar and bind the `snapshot_name` to the resulting internal template.
- (b) Restore a sandbox from a snapshot via the standard Kubernetes path: containerd issues a `RunPodSandbox` call to Kuasar's sandboxer and transparently passes through the pod annotations set in the Pod spec; Kuasar maps these incoming annotations to its internal `kuasar.io/*` representation, from which `KuasarNativeResolver` derives the restore intent.
- (c) Pause an existing running sandbox (snapshot its state + stop the VM) and later resume it in place, without re-deploying via kubelet.

### Goals

- Move snapshot/restore operations from the JSON admin socket to dedicated gRPC services as the northbound snapshot API; keep the JSON admin socket for operational interfaces only (inspect, destroy, pool management).
- Support `SandboxController.PauseSandbox` / `ResumeSandbox` as the explicit lifecycle management path for upper-layer orchestrators.
- Introduce `KuasarNativeResolver` and the `RestoreIntentResolver` trait to handle containerd-triggered restores via the CRI path.
- Support **Fork** mode (→ WarmFork autonomous restore) and **Resume** mode (→ Continuation restore) in the current scope.


### Non-Goals

- Implementing any specific orchestrator's snapshot driver (lives outside Kuasar).

---

## Background

### Existing Service API Model

`service/mod.rs` exposes a Unix-socket JSON admin protocol rooted on a `Handle<F>` struct:

```rust
pub struct Handle<F> {
    pub factory:            Arc<F>,
    pub sandboxes:          Arc<RwLock<HashMap<String, Arc<Mutex<KuasarSandbox<F::VM>>>>>>,
    pub pool:               Option<Arc<TemplatePool>>,
    pub continuation_store: Option<Arc<ContinuationStore>>,
    pub snapshot_config:    SnapshotConfig,
    pub sandbox_base_dir:   PathBuf,
}
```

Admin actions retained on the JSON socket (operational use only):

| Action | Description |
|--------|-------------|
| `sandbox-destroy` | Force-stop a sandbox and release resources |
| `sandbox-list/get` | Inspect running sandboxes |
| `template-list/get` | Inspect available templates |
| `pool-status/refill/gc` | Pool health and maintenance |
| `continuation-list/delete` | Inspect and clean up continuation entries |

The restore path triggered by CRI `RunPodSandbox` lives in `KuasarSandboxer::start()` in `sandbox.rs`, which calls `parse_snapshot_intent()` to read `kuasar.io/*` annotations.

### Snapshot/Restore API: gRPC Services

Snapshot creation and sandbox restore are handled exclusively by the two gRPC services on `/run/vmm-sandboxer-service.sock`. Snapshot operations removed from the admin socket and their gRPC equivalents:

| Removed Admin Action | gRPC Equivalent |
|---|---|
| `template-create` | `SandboxSnapshotController.CreateSandboxSnapshot` |

### Orchestrator Snapshot Semantics

Different orchestrators model snapshots differently. As a concrete example, AgentCube defines two modes:

| Mode | Semantics | Target Kuasar type |
|------|-----------|--------------------|
| `Fork` | 1:N reusable baseline; no user state at snapshot time | WarmFork, autonomous mode (`task_id=None`) |
| `Resume` | 1:1 session state capture | Continuation |

Both modes are supported by the same `CreateSandboxSnapshot` and `SandboxController.PauseSandbox` / `ResumeSandbox` gRPC interfaces via the `mode` field.

### The Two Problems

**Problem 1 — Snapshot creation path:**
Upper-layer orchestrators need to create snapshots and bind a `snapshot_name` to the resulting template. The existing `template-create` JSON action is insufficient: it is not typed, not schema-driven, and not reachable by standard Kubernetes-ecosystem tooling.

**Problem 2 — Restore path:**
`parse_snapshot_intent()` knows only `kuasar.io/*` annotations. Orchestrators that call Kuasar directly do not route through kubelet; they need an explicit, synchronous RPC to pause a running sandbox and later resume it — without tearing down and re-creating the pod via the CRI/kubelet path.

### Why gRPC for the Northbound API

The northbound callers are not only upper-layer orchestrators but also kubelet (via CRI extensions), containerd management tools (`ctr`, `nerdctl`), and `crictl`. The entire Kubernetes ecosystem uses gRPC for northbound plugin/extension APIs: CRI, CSI, CNI, Device Plugins. Kuasar already uses ttrpc (a lightweight variant) for the southbound host→guest protocol (`sandbox.proto`); **standard gRPC (HTTP/2, implemented via tonic)** is the correct choice for the northbound management API where ecosystem tooling (`grpcurl`, code generation for Go/Python clients) matters.

Transport is a dedicated Unix domain socket at `/run/vmm-sandboxer-service.sock` (configurable via `--grpc-listen <FILE>`), separate from the existing JSON admin socket (`/run/vmm-sandboxer-admin.sock`). The framing protocol is upgraded from ad-hoc JSON to standard gRPC.

---

## Design Part 1: Snapshot Creation — SandboxSnapshotController gRPC Service

### Proto Definition

Two services are co-hosted on the same Unix socket, with cleanly separated concerns:

- **`SandboxController`** — sandbox *instance* lifecycle: pause/resume running sandboxes, list, get. Defined in `sandbox.proto` (package `kuasar.sandbox.v1`).
- **`SandboxSnapshotController`** — snapshot *artifact* lifecycle: create, delete, list, plus plugin introspection. Defined in `ssi.proto` (package `ssi.v1alpha1`), the SSI standard northbound interface.

**sandbox.proto** — `SandboxController` and sandbox instance messages:

```protobuf
syntax = "proto3";
package kuasar.sandbox.v1;

service SandboxController {
    rpc PauseSandbox  (PauseSandboxRequest)   returns (PauseSandboxResponse);
    rpc ResumeSandbox (ResumeSandboxRequest)  returns (ResumeSandboxResponse);
    rpc ListSandboxes (ListSandboxesRequest)  returns (ListSandboxesResponse);
    rpc GetSandbox    (GetSandboxRequest)     returns (GetSandboxResponse);
}

// Internal snapshot mode names used within kuasar.
enum SnapshotMode {
    UNSPECIFIED  = 0;
    WARM_FORK    = 1;
    CONTINUATION = 2;
}

// PauseSandbox: snapshot the running VM and stop the CH process.
// The sandbox entry remains in the sandboxer; containerd sees it as still Running.
message PauseSandboxRequest   { string sandbox_id = 1; }
message PauseSandboxResponse  {}

// ResumeSandbox: restore the VM from the snapshot taken during PauseSandbox.
message ResumeSandboxRequest  { string sandbox_id = 1; }
message ResumeSandboxResponse {}

message ListSandboxesRequest  {}
message ListSandboxesResponse { repeated Sandbox sandboxes = 1; }

message GetSandboxRequest  { string sandbox_id = 1; }
message GetSandboxResponse { Sandbox sandbox = 1; }

message Sandbox {
    string pod_uid             = 1;
    string sandbox_id          = 2;
    string snapshot_name       = 3;
    SnapshotMode snapshot_mode = 4;
    int64 created_at_secs      = 5;  // Unix seconds since epoch
    string status              = 6;  // "created", "running", "paused", "stopped"
}
```

**ssi.proto** — `SandboxSnapshotController` (SSI northbound API):

```protobuf
syntax = "proto3";
package ssi.v1alpha1;

service SandboxSnapshotController {
    rpc GetPluginInfo         (GetPluginInfoRequest)         returns (GetPluginInfoResponse);
    rpc GetPluginCapabilities (GetPluginCapabilitiesRequest) returns (GetPluginCapabilitiesResponse);
    rpc Probe                 (ProbeRequest)                 returns (ProbeResponse);
    rpc CreateSandboxSnapshot (CreateSandboxSnapshotRequest) returns (CreateSandboxSnapshotResponse);
    rpc DeleteSandboxSnapshot (DeleteSandboxSnapshotRequest) returns (DeleteSandboxSnapshotResponse);
    rpc ListSandboxSnapshots  (ListSandboxSnapshotsRequest)  returns (ListSandboxSnapshotsResponse);
    rpc GetSandboxSnapshot    (GetSandboxSnapshotRequest)    returns (GetSandboxSnapshotResponse);
}

// UNSPECIFIED (0) is the proto3 default; in list context returns all types.
enum SnapshotMode {
    UNSPECIFIED = 0;
    FORK        = 1;  // Fork the sandbox VM state into a reusable snapshot.
    RESUME      = 2;  // Save a resumable checkpoint for continuation.
}

message CreateSandboxSnapshotRequest {
    string pod_uid       = 1;
    string snapshot_name = 2;
    SnapshotMode mode    = 3;
    // Extension fields. Well-known keys:
    //   "generation" (uint64 string, default "0"): workload generation for Resume mode.
    map<string, string> parameters = 4;
}
message CreateSandboxSnapshotResponse { SandboxSnapshot snapshot = 1; }

message DeleteSandboxSnapshotRequest  { string snapshot_name = 1; }
message DeleteSandboxSnapshotResponse {}

message GetSandboxSnapshotRequest  { string snapshot_name = 1; }
message GetSandboxSnapshotResponse { SandboxSnapshot snapshot = 1; }

// mode = UNSPECIFIED (default) lists all types.
message ListSandboxSnapshotsRequest  { SnapshotMode mode = 1; }
message ListSandboxSnapshotsResponse { repeated SandboxSnapshot snapshots = 1; }

message SandboxSnapshot {
    string pod_uid       = 1;
    string snapshot_name = 2;
    SnapshotMode mode    = 3;
}

message GetPluginInfoRequest  {}
message GetPluginInfoResponse { string name = 1; string version = 2; }

message GetPluginCapabilitiesRequest  {}
message GetPluginCapabilitiesResponse { repeated PluginCapability capabilities = 1; }

enum PluginCapabilityType {
    PLUGIN_UNKNOWN = 0;
    PLUGIN_FORK    = 1;
    PLUGIN_RESUME  = 2;
}
message PluginCapability { PluginCapabilityType type = 1; }

message ProbeRequest  {}
message ProbeResponse { bool ready = 1; }
```

### Two-Service Split: Rationale

The two services represent two distinct caller intents:

- **`SandboxSnapshotController`** — callers that manage snapshot *artifacts* (create a snapshot of a running pod, enumerate what snapshots exist, delete a snapshot). These operations act on the template store, not on running sandbox instances.
- **`SandboxController`** — callers that manage sandbox *instance lifecycle* (pause a running sandbox to snapshot its state, resume it, query status). `PauseSandbox` / `ResumeSandbox` are the explicit lifecycle management RPCs for upper-layer orchestrators that do not route through kubelet.

### CreateSandboxSnapshot Semantics

`snapshot_name` is the caller-supplied key and the single reference point for both creation and deletion.

For **Continuation** mode, `snapshot_name` may be omitted; Kuasar derives a stable key from `pod_uid` and `parameters["generation"]` so that the same workload generation always maps to the same snapshot slot.

**Handler logic** (inside `handle_create_sandbox_snapshot`):

```rust
// 1. Resolve pod_uid → internal sandbox_id via the pod_uid_index reverse map
let sandbox_id = handle.pod_uid_index.read().await.get(&req.pod_uid).cloned()
    .ok_or_else(|| anyhow!("no running sandbox found for pod_uid={}", req.pod_uid))?;

// 2. Derive snapshot key
let key = match (mode, req.snapshot_name.is_empty()) {
    (Continuation, true) => TemplateKey::from_workload_identity(&req.pod_uid, generation).key,
    _                    => req.snapshot_name.clone(),
};

// 3. Build WorkloadIdentity for Continuation mode
let workload_identity = (mode == Continuation).then(|| WorkloadIdentity {
    pod_uid:    req.pod_uid.clone(),
    generation: req.parameters.get("generation").and_then(|g| g.parse().ok()).unwrap_or(0),
});

// 4. Snapshot the running VM; persist result in TemplatePool or ContinuationStore
let template_id = new_template_id();
let tmpl = snapshot_from_sandbox(&handle, &sandbox_id, &template_id, &key,
                                  snap_type, workload_identity).await?;
```

`snapshot_from_sandbox` handles:
- Pre-snapshot WarmFork readiness check (`CheckInjectSocket`) when `mode == WARM_FORK`
- VM pause + snapshot + immediate resume so the source pod keeps running
- Persistence to `TemplatePool` (WarmFork) or `ContinuationStore` (Continuation)

---

### Handle Changes

The `Handle` struct gains a `pod_uid_index` field — a reverse map from `pod_uid` to `sandbox_id`, maintained at every sandbox create/delete event and rebuilt from persisted state on startup. This replaces per-request annotation scanning with an O(1) lookup:

```rust
pub struct Handle<F> {
    pub factory:            Arc<F>,
    pub sandboxes:          Arc<RwLock<...>>,
    pub pool:               Option<Arc<TemplatePool>>,
    pub continuation_store: Option<Arc<ContinuationStore>>,
    pub snapshot_config:    SnapshotConfig,
    pub sandbox_base_dir:   PathBuf,
    /// Reverse index: pod_uid → sandbox_id.
    /// Rebuilt from persisted state on startup; kept up-to-date at create/delete time.
    pub pod_uid_index:      Arc<RwLock<HashMap<String, String>>>,  // NEW
}
```

`KuasarSandboxer` populates `pod_uid_index` when a sandbox is created via CRI `RunPodSandbox` (which sets `kuasar.io/pod-uid` in sandbox labels) and removes the entry when the sandbox is stopped.

---

## Design Part 2: Restore — Two Paths

### Pause/Resume Path: SandboxController.PauseSandbox / ResumeSandbox

The upper-layer orchestrator issues `SandboxController.PauseSandbox` to suspend a running sandbox without tearing it down. Later, `SandboxController.ResumeSandbox` restores it in place. Both RPCs identify the sandbox by its existing `sandbox_id` — the Kubernetes-managed pod identity is fully preserved across the pause/resume cycle.

This path is fundamentally different from tearing down and re-creating the pod:
- The sandbox entry in the sandboxer is retained throughout.
- containerd's view of the sandbox is kept consistent via status spoofing (see [containerd Status Reporting](#containerd-status-reporting)).
- The running container process in the guest survives the pause; after resume, it is re-adopted by containerd without a new `runc create`.

See [Design Part 3](#design-part-3-pauseresume--vm-lifecycle-state) for the full implementation details.

### CRI-triggered Path: KuasarNativeResolver (containerd)

This feature introduces the `RestoreIntentResolver` trait and refactors `KuasarSandboxer::start()` to call a new `resolve_restore_intent()` method that iterates an ordered resolver chain. The resolver chain exists because pod annotations are not fixed: different deployments or upper-layer orchestrators may express restore intent through different annotation schemas. Adding a new resolver to the chain is sufficient to support new annotation keys — `start()` and existing resolvers are not modified.

```rust
pub struct KuasarSandboxer<F, H> {
    // ... existing fields unchanged ...

    /// Ordered resolver chain for CRI-triggered restores.
    restore_resolvers: Vec<Arc<dyn RestoreIntentResolver>>,
}
```

**Construction:**

```rust
let mut restore_resolvers: Vec<Arc<dyn RestoreIntentResolver>> =
    vec![Arc::new(KuasarNativeResolver)];
if !config.snapshot.annotation_resolvers.is_empty() {
    restore_resolvers.push(Arc::new(MappingResolver::new(
        config.snapshot.annotation_resolvers.clone(),
    )));
}
```

**`KuasarNativeResolver`** encapsulates `kuasar.io/*` annotation parsing (logic migrated from `parse_snapshot_intent()`). It returns `Ok(None)` for any other annotation key.

**`MappingResolver`** is driven by `SnapshotConfig::annotation_resolvers` — a list of rules defined in the config file. Each rule maps a pod annotation key to a `RestoreIntent`. Currently only `warmfork` mode is supported. New annotation keys are supported through config changes alone, without modifying Kuasar code.

```toml
# config.toml — AgentCube CRI-path integration example
[[sandbox.snapshot.annotation_resolvers]]
snapshot_key_annotation = "agentcube.volcano.sh/snapshot-key"
```

**Matching rules (evaluated in order):**

1. `KuasarNativeResolver` is built-in and always first — it is hardcoded in `KuasarSandboxer::new()` and cannot be removed or reordered via config. If any `kuasar.io/*` annotation is present, it owns the decision and `MappingResolver` is never reached.
2. `MappingResolver` evaluates its entries in config order. The first entry whose annotation key is found in the pod annotations wins; remaining entries are skipped.
3. If an entry's annotation key is absent, the entry is skipped and the next entry is tried.
4. If no entry matches, the resolver returns `Ok(None)` and the start flow falls through to environment snapshot or cold-start.

### Shared Restore Functions

The CRI-triggered path converges on the same internal restore functions:

| Function | Called by |
|---|---|
| `restore_sandbox_warm_fork(handle, id, snapshot_name, template_id)` | CRI-triggered path (WarmFork via KuasarNativeResolver) |
| `restore_sandbox_continuation(handle, id, pod_uid, generation, snapshot_name)` | CRI-triggered path (Continuation via KuasarNativeResolver) |
| `resume_paused_sandbox(handle, sandbox_id)` | Pause/Resume path (ResumeSandbox gRPC) |

### Modified start() Flow

```rust
async fn start(&self, id: &str) -> Result<()> {
    if self.template_pool.is_some() || self.continuation_store.is_some() {
        if self.any_snapshot_restore_enabled() {
            let intent = self.resolve_restore_intent(id).await?;

            match intent {
                RestoreIntent::WarmFork { key, template_id: Some(tid) } => {
                    return self.start_with_template_id(id, &tid, Some(&key.key)).await;
                }
                RestoreIntent::WarmFork { key, template_id: None } => {
                    return self.start_with_template_key(id, &key).await;
                }
                RestoreIntent::Continuation { identity } => {
                    return self.start_with_continuation_snapshot(id, &identity).await;
                }
                RestoreIntent::None => { /* fall through */ }
            }
        }
        // ... unchanged: environment snapshot fallback and cold-start logic ...
    }
    // ... unchanged: cold-start path ...
}
```

`start()` is only reached via the CRI-triggered path (containerd). The pause/resume path (`PauseSandbox`/`ResumeSandbox`) calls `resume_paused_sandbox()` directly, bypassing `start()`.

---

## Design Part 3: Pause/Resume — VM Lifecycle State

### Pause Semantics

`PauseSandbox` performs a **snapshot-then-kill** sequence on the running sandbox:

```
pause_sandbox(sandbox_id):
  1. snapshot_from_sandbox(sandbox_id, template_id, key=sandbox_id,
                            type=Continuation, workload_identity=Some({pod_uid, generation=0}))
     → VM is paused, memory snapshot written to ContinuationStore, VM immediately resumed.
     → Snapshot key = sandbox_id (used later by ResumeSandbox to look it up).

  2. sandbox.status = SandboxStatus::Paused

  3. sandbox.vm.stop(force=true)
     → CH process is killed. Snapshot is already on disk.
     → The sandbox entry in the sandboxer remains; only the VM process is gone.

  4. sandbox.restore.template_key = Some(sandbox_id)
     → Points ResumeSandbox to the right ContinuationStore entry.

  5. sandbox.dump()
     → Persist Paused state + template_key to disk for crash recovery.
```

The `snapshot_from_sandbox` function (shared with WarmFork snapshot creation) handles the VM pause + snapshot + resume internally; the `vm.stop(true)` call in step 3 is the definitive kill after the snapshot is safely on disk.

### Resume Semantics

`ResumeSandbox` restores the VM from the snapshot taken during `PauseSandbox`:

```
resume_paused_sandbox(sandbox_id):
  1. Verify sandbox.status == Paused.
  2. Acquire the ContinuationStore lease keyed by sandbox_id.
  3. sandbox.network.take()
     → Drop the Network handle left from the original pod's prepare_network().
     → Prevents start_from_snapshot from entering the "rebuild-network" branch
       (self.network.is_some()), which would call refresh_instance_identity() via ttrpc
       on the non-existent guest and fail.
  4. *sandbox.client.lock().await = None
     → Clear the stale ttrpc client from before the pause.
     → init_client() inside start_from_snapshot only creates a new client if client is None;
       leaving the broken client causes all ttrpc calls (adopt_container, etc.) to fail
       with SendError.
  5. sandbox.reopen_continuation_taps()
     → Reopen IFF_PERSIST tap devices in the preserved netns (same-node restore).
  6. sandbox.start_from_snapshot(src)
     → Restore VM from snapshot; internally:
       a. launch_for_restore(): remove stale task.vsock, start new CH process.
       b. restore VM memory from snapshot files.
       c. init_client(): wait up to 45 s for guest ttrpc (port 1024) to be ready,
          create fresh ttrpc client.
       d. For each orphan container in orphan_container_ids:
            adopt_container() → guest re-registers the already-running runc process.
       e. forward_events(): spawn background task calling guest's get_events() in a
          loop; publish OOM events to containerd. Non-OOM events (TaskCreate, TaskStart)
          are consumed and discarded by the guest—containerd derives container state
          from the RPC response (PID), not from events.
  7. sandbox.status transitions from Paused → Running (set inside start_from_snapshot).
```

After `start_from_snapshot` returns, when containerd next calls `Create`/`Start` on the task agent (via `task.vsock:1024`), `KuasarFactory::create()` detects the adoption record (set by `adopt_container`) and wraps the existing orphan process in a `KuasarContainer` without spawning a new `runc` process. `KuasarInitLifecycle::start()` for adopted containers skips `runc start` and immediately sets `p.state = RUNNING`.

### containerd Status Reporting

During the `Paused` state, containerd continuously polls the sandbox status. If the sandboxer returns `Paused` (mapped to NOTREADY), containerd triggers a stop-loop every ~15 seconds, which would destroy the sandbox.

To prevent this, `KuasarSandbox::status()` is overridden to lie to containerd:

```rust
fn status(&self) -> Result<SandboxStatus> {
    // Paused internally means CH is dead + snapshot on disk.
    // containerd maps Paused → NOTREADY and triggers a stop loop every ~15 s.
    // Report Running(0) so containerd sees READY and leaves the sandbox alone.
    // Internal code reads self.status directly and still sees the real Paused state.
    if matches!(self.status, SandboxStatus::Paused) {
        return Ok(SandboxStatus::Running(0));
    }
    Ok(self.status.clone())
}
```

For `stop()`, the `Paused` arm is handled explicitly:

```rust
// In stop(), when status == Paused:
//   vm.stop(force) is a no-op (CH is already dead).
//   Set status = Stopped and signal exit so wait() unblocks on pod deletion.
let was_paused = matches!(self.status, SandboxStatus::Paused);
self.vm.stop(force).await?;
self.destroy_network().await;
if was_paused {
    self.status = SandboxStatus::Stopped(0, 0);
    self.exit_signal.signal();
}
```

### Guest Task Agent: Non-blocking Event Dispatch

The guest task agent (`vmm/task`) runs the containerd-shim `TaskService` inside the VM. `send_event()` puts events (TaskCreate, TaskStart, TaskDelete, OOM) into a bounded mpsc channel (capacity 128). The host-side `forward_events()` drains this channel by calling the guest's `get_events` ttrpc method in a loop; only OOM events are forwarded to containerd.

**Problem**: `send_event()` originally used `tx.send().await`, which blocks the calling tokio task if the channel is full. If `forward_events()` fails and exits (e.g., broken connection during the window between resume and reconnect), no one drains the channel. After 128 events, every `Create`/`Start` RPC handler in the guest blocks on `send_event()`, exhausting the tokio worker pool. With no worker threads free, the ttrpc server's accept loop stops running. CH's vsock proxy never receives the guest's OK response to new connection attempts, and containerd's `hybridVsockDialer` (100-second total timeout) eventually fails.

**Fix**: change `send_event()` to use `try_send`, which never suspends:

```rust
// vmm-task, in the vendored containerd-shim crate (task.rs):
pub async fn send_event(&self, event: impl Event) {
    let topic = event.topic();
    if let Err(e) = self.tx.try_send((topic.to_string(), Box::new(event))) {
        warn!("drop event {}: {}", topic, e);
    }
}
```

With `try_send`, a full channel causes the event to be dropped and a warning logged — it never suspends the caller. Non-OOM events were already discarded by `get_events` anyway. OOM events are only dropped if the channel is full AND the consumer is absent — in that case the host cannot receive them regardless.

---

## End-to-End Flows

### WarmFork Snapshot Creation

```
Upper-layer orchestrator
  ↓ gRPC (Unix socket)
CreateSandboxSnapshot {
  pod_uid:       "uid-build-node-a",              ← Kubernetes Pod UID
  snapshot_name: "python-ready-fork-g12-r1",
  mode:          FORK,
  parameters:    {}
}
  ↓
handle_create_sandbox_snapshot():
  ├── pod_uid_index.get("uid-build-node-a") → "build-sandbox-node-a"
  ├── CheckInjectSocket (WarmFork readiness validation)
  ├── snapshot_from_sandbox("build-sandbox-node-a", template_id, key="python-ready-fork-g12-r1",
  │     type=WarmFork, workload_identity=None)
  │     → PooledTemplate added to TemplatePool
  └── return SandboxSnapshot {
        pod_uid: "uid-build-node-a",
        snapshot_name: "python-ready-fork-g12-r1",
        mode: FORK
      }
  ↓
CreateSandboxSnapshotResponse { snapshot: { ... } }
```

### WarmFork Restore — CRI Path

```
containerd
  ↓ RunPodSandbox (sandbox plugin, with pod annotations from Pod spec)
KuasarSandboxer::start(id)
  ↓
resolve_restore_intent():
  KuasarNativeResolver.resolve()
    → Ok(Some(RestoreIntent::WarmFork { key: "python-ready-fork-g12-r1" }))
  ↓
start_with_template_key(id, "python-ready-fork-g12-r1")
  ↓
WarmFork autonomous restore:
  vm.restore() → InjectTask(task_id="")
  CAPABILITIES → PREPARE("") → READY → COMMIT → STARTED
```

### Continuation Snapshot Creation

**Case 1 — explicit `snapshot_name`**: the caller supplies a name; that name becomes the storage key directly. `workload_identity` is stored as template metadata. Only the explicit gRPC restore path can reference this snapshot by name; the CRI path derives a different key from `pod_uid + generation` and will not match.

```
Upper-layer orchestrator
  ↓ gRPC
CreateSandboxSnapshot {
  pod_uid:       "uid-abc",
  snapshot_name: "session-abc-resume-k3",   ← caller-supplied key
  mode:          RESUME,
  parameters:    {"generation": "0"}
}
  ↓
handle_create_sandbox_snapshot():
  ├── pod_uid_index.get("uid-abc") → "session-abc"
  ├── key = "session-abc-resume-k3"                              ← snapshot_name used directly
  ├── workload_identity = { pod_uid: "uid-abc", generation: 0 } ← stored as metadata only
  ├── snapshot_from_sandbox("session-abc", template_id, key="session-abc-resume-k3",
  │     type=Continuation, workload_identity=Some(...))
  │     → ContinuationStore entry saved under key="session-abc-resume-k3"
  └── return SandboxSnapshot {
        pod_uid: "uid-abc",
        snapshot_name: "session-abc-resume-k3",
        mode: RESUME
      }
  ↓
CreateSandboxSnapshotResponse { snapshot: { ... } }
```

**Case 2 — `snapshot_name` omitted**: Kuasar derives the storage key from `pod_uid + generation` via `TemplateKey::from_workload_identity`. Both the CRI restore path and the explicit gRPC restore path can locate this snapshot, because both derive the same key from `workload_identity`.

```
Upper-layer orchestrator
  ↓ gRPC
CreateSandboxSnapshot {
  pod_uid:       "uid-abc",
  snapshot_name: "",                            ← omitted
  mode:          RESUME,
  parameters:    {"generation": "0"}
}
  ↓
handle_create_sandbox_snapshot():
  ├── pod_uid_index.get("uid-abc") → "session-abc"
  ├── workload_identity = { pod_uid: "uid-abc", generation: 0 } ← drives key derivation
  ├── key = TemplateKey::from_workload_identity("uid-abc", 0)   ← derived from workload_identity
  ├── snapshot_from_sandbox("session-abc", template_id, key=<derived>,
  │     type=Continuation, workload_identity=Some(...))
  │     → ContinuationStore entry saved under derived key
  └── return SandboxSnapshot {
        pod_uid: "uid-abc",
        snapshot_name: <derived>,
        mode: RESUME
      }
  ↓
CreateSandboxSnapshotResponse { snapshot: { ... } }
```

### Continuation Restore — CRI Path

```
containerd
  ↓ RunPodSandbox (with kuasar.io/continuation-snapshot=<key> or workload-identity annotations)
KuasarSandboxer::start(id)
  ↓
resolve_restore_intent():
  KuasarNativeResolver.resolve()
    → Ok(Some(RestoreIntent::Continuation { identity: { pod_uid: "uid-abc", generation: 0 } }))
  ↓
start_with_continuation_snapshot(id, &identity)
  ↓
Continuation restore: vm.restore() → guest resumes full session state
```

### Pause

```
Upper-layer orchestrator
  ↓ gRPC
PauseSandbox { sandbox_id: "sandbox-abc" }
  ↓
pause_sandbox():
  ├── snapshot_from_sandbox("sandbox-abc", template_id, key="sandbox-abc",
  │     type=Continuation, workload_identity=Some({pod_uid, generation=0}))
  │     → VM vCPUs paused, memory + disk snapshotted, VM resumed, CH still alive
  │     → ContinuationStore entry saved under key="sandbox-abc"
  ├── sandbox.status = Paused
  ├── sandbox.vm.stop(force=true)   ← kill CH now that snapshot is on disk
  ├── sandbox.restore.template_key = Some("sandbox-abc")
  └── sandbox.dump()                ← persist Paused state to disk
  ↓
PauseSandboxResponse {}

  [containerd polls status → status() returns Running(0) → no stop loop]
```

### Resume

```
Upper-layer orchestrator
  ↓ gRPC
ResumeSandbox { sandbox_id: "sandbox-abc" }
  ↓
resume_paused_sandbox():
  ├── verify status == Paused
  ├── acquire ContinuationStore lease (key="sandbox-abc")
  ├── sandbox.network.take()        ← drop stale Network handle
  ├── *sandbox.client.lock() = None ← clear stale ttrpc client
  ├── sandbox.reopen_continuation_taps()
  └── sandbox.start_from_snapshot():
        ├── launch_for_restore(): remove stale task.vsock, start CH, load snapshot
        ├── init_client(): wait for guest ttrpc port 1024, create fresh client
        ├── adopt_container(orphan_id) for each orphan
        │     → guest re-registers the running runc process
        ├── forward_events(): spawn background OOM event forwarder
        └── sandbox.status = Running(0)
  ↓
ResumeSandboxResponse {}

  [containerd calls Create/Start on task.vsock:1024]
  [KuasarFactory::create() finds adoption record → wraps orphan PID, no new runc]
  [KuasarInitLifecycle::start() for adopted: p.state = RUNNING; return Ok()]
```

---

## Responsibility Boundaries

| Component | Owns | Does Not Own |
|-----------|------|-------------|
| Upper-layer orchestrator | Calls `CreateSandboxSnapshot` / `PauseSandbox` / `ResumeSandbox` gRPC; `snapshot_name` generation | WarmFork injection protocol; snapshot files |
| Kuasar `SandboxSnapshotController` gRPC | Template creation from running sandbox; snapshot artifact listing and deletion | Orchestrator control-plane semantics; GC; sandbox instance lifecycle |
| Kuasar `SandboxController` gRPC | Sandbox instance lifecycle via Pause/Resume; `pod_uid_index` read for snapshot creation | Snapshot artifact lifecycle; annotation parsing; CRI path |
| `KuasarNativeResolver` | `kuasar.io/*` annotation resolution for containerd CRI path (logic migrated from `parse_snapshot_intent()`) | gRPC path; other annotation keys |
| `MappingResolver` | Config-driven annotation-to-intent mapping; supports WarmFork rules for arbitrary annotation keys | gRPC path; `kuasar.io/*` annotation keys |
| `start()` dispatch | Acts on `RestoreIntent` from the resolver chain; calls existing restore functions | gRPC restore path; pod_uid_index |
| `resume_paused_sandbox()` | Full Continuation restore for in-place resume: stale state cleanup, VM restore, client init, container adoption | gRPC/CRI distinction; WarmFork |
| `restore_sandbox_warm_fork()` etc. | Full WarmFork / Continuation restore (unchanged) | Caller identity; annotation or gRPC source |
| Guest task agent (`vmm/task`) | Container lifecycle in VM; non-blocking event dispatch to host via `try_send` | Event forwarding (host-side concern); container adoption triggering |

---

## Key Design Decisions

**Standard gRPC (HTTP/2) over a dedicated Unix socket.** The northbound callers include kubelet (CRI extensions), containerd management tools, and upper-layer orchestrators — the entire Kubernetes ecosystem uses gRPC for northbound plugin APIs (CRI, CSI, CNI). Both services are co-hosted on a **new** dedicated socket (`/run/vmm-sandboxer-service.sock`), separate from the existing JSON admin socket (`/run/vmm-sandboxer-admin.sock`). The implementation uses **tonic** (standard gRPC / HTTP/2), not ttrpc — ttrpc is reserved for the southbound host→guest protocol (`sandbox.proto`). Any standard gRPC client (`grpcurl`, Go/Python generated stubs) can connect without special libraries.

**`PauseSandbox` / `ResumeSandbox` as the explicit lifecycle management RPCs.** Orchestrators that call Kuasar directly do not route through kubelet. The pause/resume design treats the sandbox as a persistent entity that transitions between `Running` and `Paused` states, rather than tearing it down and creating a new one. This preserves the pod's `sandbox_id`, network identity (tap, netns), and `pod_uid` across the pause/resume cycle — the sandbox is the same object from containerd's perspective.

**Snapshot-then-kill for `PauseSandbox`.** `PauseSandbox` calls `snapshot_from_sandbox()` (same function used by WarmFork/Continuation snapshot creation), which pauses vCPUs, writes the snapshot, then resumes the VM. Only after the snapshot is safely on disk does the CH process get killed (`vm.stop(force=true)`). This guarantees no data loss: if the kill fails, the VM is still running; if the snapshot fails, the kill is never attempted.

**`status()` lies to containerd during `Paused` state.** containerd maps any non-Running sandbox status to NOTREADY and triggers a stop/restart loop every ~15 seconds. To prevent this, `KuasarSandbox::status()` returns `Running(0)` when the internal state is `Paused`. Internal code reads `self.status` directly and sees the real `Paused` state. This is the minimal-interference approach: containerd is satisfied, kubelet does not restart the pod, and the sandboxer can resume at any time without fighting containerd's reconciliation loop.

**`snapshot_name` as the single user-facing key.** Callers reference snapshots by `snapshot_name` for both creation and deletion; there is no separate `snapshot_id` exposed in the API. The `snapshot_name` maps directly to the internal pool key, so the name chosen by the caller is the name visible in `TemplatePool` and `ContinuationStore`. This eliminates the two-key indirection (`name` → `snapshot_id` → physical artifact) and removes the need for a `SnapshotKeyStore` intermediary.

**`pod_uid_index` reverse map instead of annotation scanning.** The index is a `HashMap<pod_uid, sandbox_id>` maintained at every sandbox create/delete event and rebuilt from persisted sandbox labels on startup. This gives O(1) lookup in `CreateSandboxSnapshot` (which must find a sandbox from a `pod_uid`) without scanning annotations across all sandboxes, and avoids the race window present in on-the-fly annotation lookups.

**Non-blocking `send_event()` in the guest task agent.** The guest task agent's event channel (capacity 128) is drained by the host-side `forward_events()` background task. If the consumer is absent (e.g., during the reconnect window after resume), blocking `send().await` would exhaust the guest's tokio thread pool and stall the ttrpc server's accept loop. Using `try_send` ensures event dispatch is always non-blocking: the event is dropped with a warning if the channel is full, and the RPC handler returns immediately.
