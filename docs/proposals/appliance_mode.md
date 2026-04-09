# [Proposal] Appliance Sandbox Mode for AI Agent Workloads

| Field | Value |
|-------|-------|
| **Title** | Appliance Sandbox Mode for AI Agent Workloads |
| **Authors** | <!-- Your name/handle here --> |
| **Status** | Draft |
| **Created** | 2026-02-25 |
| **Target Release** | TBD |
| **Related Issues** | N/A |

## Table of Contents

- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
  - [Use Cases](#use-cases)
- [Background](#background)
  - [Current Kuasar Architecture](#current-kuasar-architecture)
  - [Limitations for Agent Sandbox Scenarios](#limitations-for-agent-sandbox-scenarios)
  - [Industry Trends](#industry-trends)
- [Proposal](#proposal)
  - [Overview](#overview)
  - [Architecture: Three-Layer Engine Design](#architecture-three-layer-engine-design)
  - [Vmm Trait: Multi-VMM Abstraction](#vmm-trait-multi-vmm-abstraction)
  - [GuestRuntime Trait: Pluggable Guest Communication](#guestruntime-trait-pluggable-guest-communication)
  - [Appliance Readiness Protocol](#appliance-readiness-protocol)
  - [API Adapters: K8s and Direct](#api-adapters-k8s-and-direct)
  - [kuasar-builder: Two-Phase Template Build Pipeline](#kuasar-builder-two-phase-template-build-pipeline)
  - [Admission Controller](#admission-controller)
- [Detailed Design](#detailed-design)
  - [Vmm Trait Definition](#vmm-trait-definition)
  - [GuestRuntime Trait Definition](#guestruntime-trait-definition)
  - [SandboxEngine Core](#sandboxengine-core)
  - [Runtime Snapshot Lifecycle](#runtime-snapshot-lifecycle)
  - [Boot Mode Selection: Benchmark-Informed Heuristics](#boot-mode-selection-benchmark-informed-heuristics)
  - [Artifact-to-Start-Mode Mapping](#artifact-to-start-mode-mapping)
  - [RootfsProvider Trait: Pluggable Disk Backend](#rootfsprovider-trait-pluggable-disk-backend)
  - [Snapshot Template Version Validation](#snapshot-template-version-validation)
  - [K8s Adapter: Sandbox and Task API Mapping](#k8s-adapter-sandbox-and-task-api-mapping)
  - [Direct Adapter: Native Sandbox API](#direct-adapter-native-sandbox-api)
  - [Appliance Protocol Specification](#appliance-protocol-specification)
  - [Configuration](#configuration)
  - [Observability and Events](#observability-and-events)
- [Compatibility](#compatibility)
  - [Backward Compatibility](#backward-compatibility)
  - [K8s and containerd Compatibility](#k8s-and-containerd-compatibility)
  - [VMM Compatibility Matrix](#vmm-compatibility-matrix)
  - [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints)
- [Implementation Plan](#implementation-plan)
  - [Milestones](#milestones)
  - [Code Changes Overview](#code-changes-overview)
- [Alternatives Considered](#alternatives-considered)
- [References](#references)

---

## Summary

This proposal introduces an **Appliance Sandbox Mode** for Kuasar, targeting AI Agent and serverless workloads that require sub-second microVM startup. In appliance mode, a single microVM runs a single application process — there is no guest agent (`vmm-task`), no container abstraction, and no `exec`/`attach` capability. The VM itself **is** the application.

To support this alongside Kuasar's existing standard mode, we propose a **three-layer engine architecture** with pluggable VMM backends (Cloud Hypervisor, Firecracker), pluggable guest runtime strategies (standard `vmm-task` vs. appliance `READY` protocol), and pluggable API adapters (K8s/containerd vs. direct gRPC). The runtime mode and VMM backend are selected at process startup, with zero runtime branching.

Additionally, a **kuasar-builder** subproject is introduced to provide a two-phase `OCI image → fast-boot template` build pipeline, producing both **Image Products** (for cold boot) and **Snapshot Products** (for snapshot-based restore). Benchmark data from CH v50.0 shows that cold boot can be 3× faster than snapshot restore for lightweight workloads, so the design supports both modes as first-class start paths with caller-driven mode selection.

The engine also provides a **runtime snapshot lifecycle** (`pause_sandbox` / `resume_sandbox` / `snapshot_sandbox`) that enables user-initiated pause/resume, BMS resource rebalancing, and node drain scenarios. Runtime snapshots are a first-class citizen alongside template snapshots — they can be chunk-ified and uploaded to an external content delivery system for cross-node restore, with content-addressed dedup providing 20–40× storage reduction for homogeneous workloads.

---

## Motivation

### Goals

1. **Sub-second sandbox startup** for AI Agent and serverless workloads (P95 < 1s from request to application ready).
2. **Appliance mode** where one microVM = one application, with no guest agent overhead.
3. **Multi-VMM support** enabling users to choose between Cloud Hypervisor and Firecracker based on their tradeoff preferences (feature richness vs. minimal latency).
4. **Preserve standard mode** for full K8s/containerd compatibility, ensuring Kuasar can serve both audiences from a single codebase.
5. **Snapshot-based fast restore** with a built-in template build pipeline (kuasar-builder).
6. **Runtime snapshot lifecycle** for pause/resume, BMS migration, and node drain — runtime snapshots are first-class citizens that can be chunk-ified for cross-node restore.
7. **Clean architecture** that separates API adaptation, core engine logic, and VMM-specific code into distinct layers.

### Non-Goals

1. **Live migration** (zero-downtime, memory pre-copy) of running VMs across nodes (future work). Note: stop-and-copy migration via runtime snapshot (pause → snapshot → chunk upload → restore on target) **is** in scope — it requires brief downtime but is architecturally simpler than live migration.
2. **Built-in image lazy loading** — content delivery (chunked images, multi-tier caching) is intentionally out of scope for Kuasar itself. It can be provided by external systems through a pluggable disk path interface.
3. **Multi-container pods in appliance mode** — appliance mode is explicitly single-application. Multi-container pods remain supported in standard mode.
4. **Cross-VMM snapshot compatibility** — CH snapshots cannot be loaded by Firecracker and vice versa. Templates are tagged with their target VMM.

### Use Cases

#### UC1: AI Agent Sandbox

An AI agent orchestration platform needs to spin up isolated execution environments for AI agents. Each agent runs a pre-built application image. Requirements:

- Cold start < 1s (from API call to agent ready)
- Strong isolation (hardware virtualization)
- No need for `kubectl exec` or multi-container support
- High concurrency (hundreds of sandboxes per node)

#### UC2: Serverless Function Execution

A FaaS platform uses microVMs for function isolation. Each function invocation gets its own VM, restored from a pre-warmed snapshot:

- Restore + resume + ready < 200ms
- Single entry process per VM
- Short-lived (seconds to minutes)

#### UC3: Standard K8s Pod Isolation (Existing)

The existing use case where Kuasar provides VM-based pod isolation for Kubernetes. Full container lifecycle support via `vmm-task`:

- `exec`/`attach`/multi-container support required
- Integration with containerd via shimv2
- This continues to work unchanged

#### UC4: Sandbox Pause/Resume and Cross-Node Migration

An AI agent platform needs to pause idle sandboxes to free resources and resume them on demand (possibly on a different node):

- User pauses agent → runtime snapshot created → memory chunk-ified and uploaded
- User resumes agent → chunks fetched → snapshot restored → agent resumes from exact state
- BMS rebalancing: platform moves sandboxes between nodes via runtime snapshot
- Node retirement: idle sandboxes snapshotted and drained before decommission
- Content-addressed dedup keeps storage cost manageable (20–40× reduction for homogeneous workloads)

---

## Background

### Current Kuasar Architecture

Kuasar today implements the containerd `Sandboxer` trait, managing VM-based sandboxes with a guest agent (`vmm-task`) that communicates via ttrpc over vsock:

```
containerd → shimv2 → KuasarSandboxer → CloudHypervisorVM
                                              │
                                        [vsock/ttrpc]
                                              │
                                         vmm-task (guest)
                                          ├── check()
                                          ├── setup_sandbox()
                                          ├── create_container()
                                          ├── exec_process()
                                          └── ...
```

Key characteristics:
- Guest runs `vmm-task` as PID1, providing a container runtime inside the VM.
- Host communicates with guest via ttrpc (`SandboxServiceClient`).
- Supports full OCI container lifecycle: create, start, exec, attach, kill, wait, stats.
- VMM is always Cloud Hypervisor, always cold-booted.

### Limitations for Agent Sandbox Scenarios

For AI Agent / serverless workloads, the current architecture introduces unnecessary overhead:

| Overhead | Impact | Appliance Mode Savings |
|----------|--------|----------------------|
| `vmm-task` guest agent startup | ~50-100ms | Eliminated entirely |
| ttrpc connection establishment | ~10-20ms | Eliminated entirely |
| `setup_sandbox()` RPC | ~5-10ms | Eliminated entirely |
| `create_container()` (namespace/cgroup) | ~10-30ms | Eliminated entirely |
| `start_process()` (fork+exec) | ~5-10ms | Eliminated entirely |
| `virtiofsd` process | ~10-20ms + ongoing overhead | Eliminated entirely |
| **Total removable overhead** | **~90-190ms** | **Eliminated** |

Additionally:
- **Snapshot restore** is not supported in the current start path (always cold boot).
- **Only Cloud Hypervisor** is supported; Firecracker's lighter-weight restore is unavailable.
- **exec/attach** capabilities add complexity but are never used in agent scenarios.

### Industry Trends

The "one VM = one application" pattern is gaining adoption across the industry:

- **AWS Firecracker** (Lambda/Fargate): Each microVM runs a single function or task. No guest-side container runtime.
- **Modal.com**: Purpose-built microVM sandbox for AI workloads. Snapshot-based restore with lazy content loading.
- **Fly.io**: Firecracker-based application VMs. Each VM is a single application.
- **Unikernel movement** (MirageOS, OSv, UniKraft): Compiles application + OS into a single bootable image — the most extreme form of the appliance model.

The term "appliance" originates from the VMware era's "Virtual Appliance" concept — a pre-configured VM image that does one thing and is managed as a black box — and has been refined through the unikernel and microVM communities into the "one VM = one application" pattern seen in modern serverless platforms.

---

## Proposal

### Overview

We propose refactoring Kuasar's internal architecture into three layers, introducing appliance mode as a first-class runtime option alongside the existing standard mode:

```
┌─────────────────────────────────────────────────────────┐
│  Layer 3: API Adapters (pluggable, select one at startup)│
│                                                         │
│  ┌───────────────────┐     ┌─────────────────────────┐  │
│  │ K8s Adapter       │     │ Direct Adapter          │  │
│  │ (shimv2+Task API) │     │ (native gRPC)           │  │
│  └─────────┬─────────┘     └───────────┬─────────────┘  │
│            └────────────┬──────────────┘                 │
│                         ▼                                │
├─────────────────────────────────────────────────────────┤
│  Layer 2: Core Engine                                    │
│                                                         │
│  SandboxEngine<V: Vmm, R: GuestRuntime>                 │
│  ├── Sandbox lifecycle: create/start/stop/delete        │
│  ├── Guest operations: delegated to GuestRuntime trait  │
│  ├── Admission Controller                               │
│  └── Template Manager                                   │
│                                                         │
├─────────────────────────────────────────────────────────┤
│  Layer 1: Pluggable Backends (select one at startup)     │
│                                                         │
│  Vmm trait              GuestRuntime trait               │
│  ┌────────────────┐     ┌────────────────────┐          │
│  │CloudHypervisor │ OR  │ VmmTaskRuntime     │ OR       │
│  ├────────────────┤     │ (ttrpc → vmm-task) │          │
│  │Firecracker     │     ├────────────────────┤          │
│  └────────────────┘     │ ApplianceRuntime   │          │
│                         │ (vsock JSON Lines) │          │
│                         └────────────────────┘          │
└─────────────────────────────────────────────────────────┘
```

**Process startup determines the combination.** Four valid configurations:

| VMM | Runtime Mode | Adapter | Primary Use Case |
|-----|-------------|---------|------------------|
| Cloud Hypervisor | Standard | K8s Adapter | K8s pod isolation (existing) |
| Cloud Hypervisor | Appliance | Direct Adapter | AI Agent sandbox (rich VMM features) |
| Firecracker | Standard | K8s Adapter | K8s pod isolation (lightweight) |
| Firecracker | Appliance | Direct Adapter | Serverless / FaaS (minimal latency) |

### Architecture: Three-Layer Engine Design

**Layer 1 (VMM + GuestRuntime)** provides hardware abstraction:
- `Vmm` trait abstracts VM lifecycle operations (create, boot, restore, resume, pause, stop, device management).
- `GuestRuntime` trait abstracts guest-side communication (readiness detection, container operations, process management).

**Layer 2 (Core Engine)** implements sandbox lifecycle logic independent of any specific VMM or runtime mode:
- Admission control (concurrency limits, budget enforcement).
- Template management (registration, selection, garbage collection).
- Start mode negotiation (auto/restore/boot with degradation).

**Layer 3 (API Adapters)** translates external protocols to engine calls:
- K8s Adapter: implements `Sandboxer` trait + Task API, mapping to engine operations.
- Direct Adapter: exposes native gRPC API with full engine capabilities.

### Vmm Trait: Multi-VMM Abstraction

The `Vmm` trait provides a unified interface for different VMMs:

```rust
trait Vmm: Send + Sync {
    async fn create(&mut self, config: VmConfig) -> Result<()>;
    async fn boot(&mut self) -> Result<()>;
    async fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()>;
    async fn resume(&mut self) -> Result<()>;
    async fn pause(&mut self) -> Result<()>;
    async fn stop(&mut self, force: bool) -> Result<()>;
    async fn wait_exit(&self) -> Result<ExitInfo>;
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;
    fn add_network(&mut self, net: NetworkConfig) -> Result<()>;
    fn vsock_path(&self) -> Result<String>;
    fn capabilities(&self) -> VmmCapabilities;
}
```

Each VMM implements this trait with its specific API:
- **Cloud Hypervisor**: CLI-based restore (`--restore source_url=`), REST API for resume/pause/snapshot, supports hot-plug and resize.
- **Firecracker**: API-based snapshot load (`PUT /snapshot/load`), PATCH-based resume, supports diff snapshots, no hot-plug.

The engine queries `VmmCapabilities` to handle differences gracefully (e.g., skipping hot-plug on Firecracker).

### GuestRuntime Trait: Pluggable Guest Communication

The `GuestRuntime` trait abstracts how the host communicates with the guest:

```rust
trait GuestRuntime: Send + Sync {
    async fn wait_ready(&self, sandbox_id: &str) -> Result<ReadyResult>;
    async fn create_container(&self, sandbox_id: &str, spec: ContainerSpec)
        -> Result<ContainerInfo>;
    async fn start_process(&self, sandbox_id: &str, container_id: &str)
        -> Result<ProcessInfo>;
    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo>;
    async fn kill_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32, signal: u32) -> Result<()>;
    async fn wait_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32) -> Result<ExitStatus>;
    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats>;
}
```

Two implementations:

- **VmmTaskRuntime** (standard mode): Connects to guest `vmm-task` via ttrpc over vsock. All methods perform real guest-side operations (namespace creation, process fork+exec, cgroup management, etc.).
- **ApplianceRuntime** (appliance mode): Uses vsock JSON Lines protocol. `wait_ready()` waits for `READY` message. `create_container()`/`start_process()` are no-ops (return pid=1). `exec_process()` returns `Unimplemented`. `kill_process()` sends `SHUTDOWN` message.

### Appliance Readiness Protocol

In appliance mode, guest-host communication uses a minimal JSON Lines protocol over vsock:

**Guest → Host:**

| Message | Semantics | Required Fields |
|---------|-----------|----------------|
| `READY` | Application is ready to serve | `sandbox_id` |
| `HEARTBEAT` | Liveness signal | `sandbox_id` |
| `METRICS` | Optional telemetry | `sandbox_id`, metric fields |
| `FATAL` | Unrecoverable error, host should reclaim VM | `sandbox_id`, `reason` |

**Host → Guest:**

| Message | Semantics | Required Fields |
|---------|-----------|----------------|
| `SHUTDOWN` | Graceful termination request | `deadline_ms` |
| `CONFIG` | One-time configuration injection | config payload |
| `PING` | Connectivity probe | — |

Example READY message:
```json
{"type":"READY","sandbox_id":"sb-123","app_version":"1.2.3"}
```

### API Adapters: K8s and Direct

**K8s Adapter** provides full containerd compatibility:
- Implements `Sandboxer` trait: `create`/`start`/`stop`/`shutdown`/`sandbox` methods map to engine sandbox operations.
- Implements Task API: In standard mode, delegates to engine → `VmmTaskRuntime` → ttrpc → `vmm-task` (real operations). In appliance mode, `Create`/`Start` are no-ops, `Exec` returns `UNIMPLEMENTED`, `Kill` maps to `SHUTDOWN`.
- Publishes containerd events (`TaskStart`, `TaskExit`, etc.).

**Direct Adapter** provides a native, minimal gRPC API with no Task/Container concepts. Designed for platforms that manage sandbox lifecycle directly (e.g., AI agent orchestrators, custom FaaS platforms).

### kuasar-builder: Two-Phase Template Build Pipeline

A new subproject providing an `OCI image → fast-boot template` pipeline. The pipeline has **two independent phases**, each producing artifacts that are useful on their own:

```
Phase 1 (Image Build):
  OCI image ──► flatten layers ──► rootfs.ext4 + image-ref.txt
                                    └──► (optional) blockmap + manifest + chunkstore

Phase 2 (Snapshot Build — optional, consumes Phase 1 output):
  rootfs.ext4 ──► boot VM ──► wait READY ──► pause + snapshot
                                               ├── CH snapshot bundle (config.json, state.json)
                                               ├── base-disk/ (qcow2 base chunked into blockmap/chunkstore)
                                               ├── memory-disk/ (memory file chunked, shared chunkstore)
                                               ├── overlay.qcow2
                                               └── snapshot.meta.json
```

**Phase 1: Image Build** produces the **Image Product**:
1. **Rootfs flattening**: OCI layers → rootfs directory (controlled mtime/uid/gid/sort order for determinism).
2. **Ext4 image**: rootfs directory → `rootfs.ext4` raw block image + `image-ref.txt`.
3. **(Optional) Chunking**: `rootfs.ext4` → content-addressed `blockmap/manifest/chunkstore` for on-demand disk loading via block-agent/artifactd. Enabled with `--emit-chunks`.

Phase 1 output alone is sufficient for **cold boot mode** — Kuasar can boot a VM directly from `rootfs.ext4` without any snapshot. When `--emit-chunks` is enabled, cold boot can also use lazy disk loading via the external block-agent FUSE backend.

**Phase 2: Snapshot Build** produces the **Snapshot Product**:
1. **Base disk chunking**: Convert `rootfs.ext4` → qcow2 base, then chunk the qcow2 into `base-disk/` (blockmap + manifest + chunkstore).
2. **VM boot**: Start VM using the selected VMM (CH or FC) with the chunked base disk (via block-agent) or local rootfs.ext4.
3. **Wait READY**: Wait for the application to initialize and report readiness via the appliance protocol.
4. **Quiesce + Snapshot**: Pause the VM, create the VMM-specific snapshot bundle, chunk the memory file into `memory-disk/`, and write `snapshot.meta.json` with version bindings.

Phase 2 supports two **disk backend modes** during the build:
- `block-agent` (default): Converts rootfs to qcow2 base → chunks it → mounts via block-agent FUSE. This is the production path that exercises the full lazy-loading stack and produces content-addressed artifacts.
- `local`: Uses rootfs.ext4 directly with a local qcow2 overlay. Simpler for debugging and development.

Phase 2 also supports two **snapshot build modes**:
- `vm` (production): Boots a real VM, waits for READY, creates an actual CH/FC snapshot.
- `synthetic` (testing): Writes placeholder snapshot files without booting a VM. Useful for CI pipelines and integration tests that need the artifact structure without requiring `/dev/kvm`.

The `snapshot.meta.json` produced by Phase 2 binds `ch_version`, kernel/initrd digest, and agent version — these are validated by Kuasar before attempting restore (see [Snapshot Template Version Validation](#snapshot-template-version-validation)).

**Pluggable disk integration**: The rootfs build step supports a **pluggable `RootfsProvider` interface** (see [RootfsProvider Trait](#rootfsprovider-trait-pluggable-disk-backend)), allowing external content delivery systems to provide alternative disk image formats without coupling Kuasar to any specific image format or caching strategy.

### Admission Controller

A node-level admission controller governs concurrent sandbox operations:

- **In-flight start limit**: Maximum concurrent sandbox starts per node.
- **Budget enforcement**: Per-sandbox `restore_timeout_ms`, `ready_timeout_ms`.
- **Degradation policy**: When limits are exceeded, reject or queue new starts with backpressure.

---

## Detailed Design

### Vmm Trait Definition

```rust
/// VMM lifecycle abstraction — completely independent of runtime mode.
trait Vmm: Send + Sync {
    /// Create a VM instance (allocate resources, do not start).
    async fn create(&mut self, config: VmConfig) -> Result<()>;

    /// Cold boot the VM.
    async fn boot(&mut self) -> Result<()>;

    /// Restore from a snapshot (VM is left in paused state).
    async fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()>;

    /// Resume a paused VM.
    async fn resume(&mut self) -> Result<()>;

    /// Pause the VM.
    async fn pause(&mut self) -> Result<()>;

    /// Create a snapshot of the current VM state.
    /// VM must be in paused state. The snapshot is written to `dest`.
    /// CH: PUT /api/v1/vm.snapshot { "destination_url": "file://<dest>" }
    /// FC: PUT /snapshot/create { "snapshot_path": "<dest>", "mem_file_path": "..." }
    async fn snapshot(&mut self, dest: &SnapshotDest) -> Result<SnapshotInfo>;

    /// Stop the VM. If force=true, SIGKILL immediately.
    async fn stop(&mut self, force: bool) -> Result<()>;

    /// Wait for the VM process to exit, return exit info.
    async fn wait_exit(&self) -> Result<ExitInfo>;

    /// Add a block device. Must be called before boot/restore.
    fn add_disk(&mut self, disk: DiskConfig) -> Result<()>;

    /// Add a network device. Must be called before boot/restore.
    fn add_network(&mut self, net: NetworkConfig) -> Result<()>;

    /// Return the vsock path for host-guest communication.
    fn vsock_path(&self) -> Result<String>;

    /// Query VMM capabilities for graceful degradation.
    fn capabilities(&self) -> VmmCapabilities;
}

struct VmmCapabilities {
    hot_plug_disk: bool,
    hot_plug_net: bool,
    hot_plug_cpu: bool,
    hot_plug_mem: bool,
    pmem_dax: bool,
    diff_snapshot: bool,
    resize: bool,
}
```

VMM capability matrix:

| Capability | Cloud Hypervisor | Firecracker |
|-----------|-----------------|-------------|
| `hot_plug_disk` | ✅ | ❌ |
| `hot_plug_net` | ✅ | ❌ |
| `hot_plug_cpu` | ✅ | ❌ |
| `hot_plug_mem` | ✅ | ❌ |
| `pmem_dax` | ✅ | ❌ |
| `diff_snapshot` | ❌ | ✅ |
| `runtime_snapshot` | ✅ (`PUT /vm.snapshot`) | ✅ (`PUT /snapshot/create`) |
| `resize` | ✅ | ❌ |
| Typical restore latency | ~1.4–1.8s (4 GB VM, see [CH v50.0 Constraints](#ch-v500-implementation-constraints)) | ~5-25ms |

### GuestRuntime Trait Definition

```rust
/// Guest-side operation abstraction — different implementations for
/// different runtime modes.
trait GuestRuntime: Send + Sync {
    /// Wait for guest to become ready.
    /// Standard: ttrpc check() + setup_sandbox().
    /// Appliance: wait for READY JSON message on vsock.
    async fn wait_ready(&self, sandbox_id: &str) -> Result<ReadyResult>;

    /// Create a container inside the VM.
    /// Standard: ttrpc create_container() — real namespace/cgroup/mount.
    /// Appliance: no-op, return pid=1.
    async fn create_container(&self, sandbox_id: &str, spec: ContainerSpec)
        -> Result<ContainerInfo>;

    /// Start the main process inside a container.
    /// Standard: ttrpc start_process() — real fork+exec.
    /// Appliance: no-op, return pid=1.
    async fn start_process(&self, sandbox_id: &str, container_id: &str)
        -> Result<ProcessInfo>;

    /// Execute an additional process inside a container.
    /// Standard: ttrpc exec_process() — real setns+fork+exec.
    /// Appliance: return Err(Unimplemented).
    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo>;

    /// Send a signal to a process.
    /// Standard: ttrpc signal_process().
    /// Appliance: send SHUTDOWN message via vsock.
    async fn kill_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32, signal: u32) -> Result<()>;

    /// Wait for a process to exit.
    /// Standard: ttrpc wait_process().
    /// Appliance: wait for VM exit.
    async fn wait_process(&self, sandbox_id: &str, container_id: &str,
                          pid: u32) -> Result<ExitStatus>;

    /// Get container resource stats.
    /// Standard: ttrpc get_stats() — read guest cgroups.
    /// Appliance: read host-side cgroups of the VMM process.
    async fn container_stats(&self, sandbox_id: &str, container_id: &str)
        -> Result<ContainerStats>;
}
```

### SandboxEngine Core

```rust
/// Core engine parameterized by VMM backend and guest runtime.
/// All four mode combinations use this same struct.
struct SandboxEngine<V: Vmm, R: GuestRuntime> {
    vmm_factory: VmmFactory<V>,
    runtime: R,
    admission: AdmissionController,
    templates: TemplateManager,
    sandboxes: HashMap<String, SandboxInstance<V>>,
}

impl<V: Vmm, R: GuestRuntime> SandboxEngine<V, R> {
    /// Start a sandbox: restore or boot, then wait for readiness.
    /// This logic is identical for all mode/VMM combinations.
    async fn start_sandbox(&mut self, id: &str, mode: StartMode)
        -> Result<StartResult>
    {
        // 1. Admission check
        self.admission.check(id)?;
        let sandbox = self.sandboxes.get_mut(id)?;
        let t0 = Instant::now();

        // 2. Resolve effective start mode
        let effective_mode = match mode {
            StartMode::Auto => {
                match self.templates.get_snapshot(id) {
                    Ok(snap) => {
                        // Validate snapshot template compatibility before restore
                        match self.templates.validate_compat(&snap, &sandbox.vmm) {
                            Ok(()) => StartMode::RestoreEager,
                            Err(e) => {
                                self.emit_event("sandbox.start.degraded", &[
                                    ("reason", "template_compat_failed"),
                                    ("error", &e.to_string()),
                                ]);
                                StartMode::Boot
                            }
                        }
                    }
                    Err(_) => StartMode::Boot,
                }
            }
            other => other,
        };

        // 3. VMM start (via Vmm trait — VMM-agnostic)
        match effective_mode {
            StartMode::RestoreEager => {
                let snap = self.templates.get_snapshot(id)?;
                sandbox.vmm.restore(&snap).await?;
                sandbox.vmm.resume().await?;
            }
            StartMode::Boot => {
                sandbox.vmm.boot().await?;
            }
            _ => unreachable!("Auto resolved above"),
        }

        // 4. Wait for readiness (via GuestRuntime trait — mode-agnostic)
        let ready = self.runtime.wait_ready(id).await?;

        Ok(StartResult {
            ready_ms: t0.elapsed().as_millis() as u64,
            mode_used: effective_mode,
            ready_at: ready.timestamp,
        })
    }

    /// Guest operations delegate to the GuestRuntime trait.
    /// In standard mode: real ttrpc calls to vmm-task.
    /// In appliance mode: no-ops or UNIMPLEMENTED.
    async fn exec_process(&self, sandbox_id: &str, container_id: &str,
                          spec: ExecSpec) -> Result<ProcessInfo> {
        self.runtime.exec_process(sandbox_id, container_id, spec).await
    }

    // --- Runtime Snapshot Lifecycle (see dedicated section below) ---

    /// Pause a running sandbox, optionally creating a runtime snapshot.
    async fn pause_sandbox(&mut self, id: &str, opts: PauseOptions)
        -> Result<PauseResult>;

    /// Resume a paused sandbox, or restore from a runtime snapshot.
    async fn resume_sandbox(&mut self, id: &str, opts: ResumeOptions)
        -> Result<ResumeResult>;

    /// Explicitly create a runtime snapshot without pausing for external use.
    async fn snapshot_sandbox(&mut self, id: &str, dest: SnapshotDest)
        -> Result<SnapshotResult>;
}
```

**Key design point**: The `Auto` mode does **not** unconditionally prefer snapshot restore. It first validates template compatibility (version domain match) before attempting restore — an incompatible or missing template triggers a graceful degradation to cold boot with a `sandbox.start.degraded` event. The caller (scheduler/orchestrator) is responsible for choosing the optimal `StartMode` based on workload characteristics and benchmark data (see [Boot Mode Selection](#boot-mode-selection-benchmark-informed-heuristics)).

### Runtime Snapshot Lifecycle

Runtime snapshots enable pause/resume, cross-node migration, and node drain. Unlike template snapshots (which are created at build time from a clean post-`READY` state), runtime snapshots capture arbitrary in-flight state of a running VM.

**Snapshot type taxonomy**:

| Type | Scope | Created by | Typical Use |
|---|---|---|---|
| **Template Snapshot** | Image-level, shared | kuasar-builder (build time) | First launch of new instances |
| **Runtime Snapshot** | Instance-specific | `pause_sandbox` / `snapshot_sandbox` (run time) | Pause/resume, migration, drain |

**Key distinction**: A VM originally started via `ColdBoot` (no template baseline) produces a runtime snapshot containing **full guest memory** — there is no template to diff against. Content-addressed dedup (zero pages, kernel text, shared libraries) is the primary storage optimization.

**Type definitions**:

```rust
struct PauseOptions {
    create_snapshot: bool,         // Create a persistent runtime snapshot
    snapshot_chunk_enabled: bool,  // Chunk-ify and upload to external content store
    drain_timeout_ms: u64,         // Wait for in-flight requests before force-pause
}

struct PauseResult {
    paused_at: Timestamp,
    snapshot_ref: Option<String>,       // e.g. "snapshot://rt-sb-123-1709308800"
    snapshot_type: String,              // "runtime"
    chunk_upload_status: Option<String>, // "completed" | "failed" | "skipped"
    chunks_total: u64,
    chunks_uploaded: u64,
    chunks_deduped: u64,
}

struct ResumeOptions {
    snapshot_ref: Option<String>,       // None = resume from in-memory paused state
    lifecycle_phase: LifecyclePhase,
}

enum LifecyclePhase {
    FirstLaunch,
    UserPauseResume,
    BMSMigration,
    NodeDrain,
}

struct ResumeResult {
    ready_ms: u64,
    mode_used: StartMode,
    snapshot_type: String,              // "template" | "runtime"
    lifecycle_phase: LifecyclePhase,
}

struct SnapshotDest {
    output_dir: String,
    chunk_enabled: bool,
}

struct SnapshotResult {
    snapshot_ref: String,
    meta_path: String,
    chunk_upload_status: Option<String>,
}
```

**`pause_sandbox` flow**:

```rust
async fn pause_sandbox(&mut self, id: &str, opts: PauseOptions) -> Result<PauseResult> {
    let sandbox = self.sandboxes.get_mut(id)?;

    // 1. Drain: best-effort wait for in-flight requests
    if opts.drain_timeout_ms > 0 {
        tokio::time::timeout(
            Duration::from_millis(opts.drain_timeout_ms),
            sandbox.drain_requests(),
        ).await.ok(); // timeout is not an error — force-pause proceeds
    }

    // 2. Pause VM (via Vmm trait — VMM-agnostic)
    sandbox.vmm.pause().await?;

    let mut result = PauseResult { paused_at: Timestamp::now(), ..Default::default() };

    // 3. Optionally create runtime snapshot
    if opts.create_snapshot {
        let dest = SnapshotDest { /* ... */ };
        let snap_info = sandbox.vmm.snapshot(&dest).await?;
        result.snapshot_ref = Some(snap_info.snapshot_ref);
        result.snapshot_type = "runtime".into();

        // 4. Optionally chunk-ify and upload (external content delivery)
        if opts.snapshot_chunk_enabled {
            match self.chunk_and_upload(id, &snap_info).await {
                Ok(stats) => {
                    result.chunk_upload_status = Some("completed".into());
                    result.chunks_total = stats.total;
                    result.chunks_uploaded = stats.uploaded;
                    result.chunks_deduped = stats.deduped;
                }
                Err(e) => {
                    // Degradation: chunk upload failed, snapshot file still intact
                    warn!("chunk upload failed: {}, falling back to full file", e);
                    result.chunk_upload_status = Some("failed".into());
                }
            }
        }
    }
    Ok(result)
}
```

**Chunk upload integration**: The `chunk_and_upload` helper calls an external content delivery system (e.g., artifactd) via HTTP:
1. Slice `memory-ranges` file into fixed-size chunks (512 KiB)
2. `POST /v1/chunks/exists` — batch check which chunks already exist (dedup)
3. `PUT /v1/chunks/{chunk_id}` — upload only new chunks (idempotent, content-addressed)
4. Write `memory.blockmap.json` + `runtime_snapshot.meta.json`

This is an **optional, degradation-safe** integration. If the external content store is unavailable, the snapshot file remains intact on local disk and can be used for same-node resume. The chunk upload failure is recorded as an event (`sandbox.snapshot.runtime.chunk_failed`) but does not fail the pause operation.

**Design rationale — why chunk logic lives in Kuasar, not the external content system**: The chunk-ify trigger is embedded in the `pause_sandbox` flow, controlled by the engine. If the content system had to trigger chunk-ify, it would need to understand VM lifecycle (which sandbox is pausing), violating the separation of concerns. Kuasar performs the chunking and HTTP uploads; the content system handles storage, dedup, distribution, and cross-node availability.

Process startup wiring (four combinations, zero runtime branching):

```rust
fn main() {
    let config = load_config();

    match (config.vmm_type, config.runtime_mode) {
        (Vmm::CloudHypervisor, Mode::Standard) => {
            let engine = SandboxEngine::new(
                VmmFactory::<CloudHypervisorVmm>::new(config.ch),
                VmmTaskRuntime::new(config.ttrpc),
            );
            K8sAdapter::new(engine).serve();
        }
        (Vmm::CloudHypervisor, Mode::Appliance) => {
            let engine = SandboxEngine::new(
                VmmFactory::<CloudHypervisorVmm>::new(config.ch),
                ApplianceRuntime::new(config.vsock),
            );
            DirectAdapter::new(engine).serve();
        }
        (Vmm::Firecracker, Mode::Standard) => {
            let engine = SandboxEngine::new(
                VmmFactory::<FirecrackerVmm>::new(config.fc),
                VmmTaskRuntime::new(config.ttrpc),
            );
            K8sAdapter::new(engine).serve();
        }
        (Vmm::Firecracker, Mode::Appliance) => {
            let engine = SandboxEngine::new(
                VmmFactory::<FirecrackerVmm>::new(config.fc),
                ApplianceRuntime::new(config.vsock),
            );
            DirectAdapter::new(engine).serve();
        }
    }
}
```

### K8s Adapter: Sandbox and Task API Mapping

```rust
// Sandboxer trait — maps to engine sandbox operations
impl Sandboxer for K8sAdapter {
    async fn create(&self, id: &str, info: SandboxOption) -> Result<()> {
        let config = self.parse_sandbox_config(info)?;
        self.engine.create_sandbox(id, config).await
    }
    async fn start(&self, id: &str) -> Result<()> {
        self.engine.start_sandbox(id, StartMode::Auto).await?;
        self.publish_event(TaskStart { pid: 1, container_id: id }).await;
        Ok(())
    }
    async fn stop(&self, id: &str, force: bool) -> Result<()> {
        let deadline = if force { 0 } else { self.graceful_timeout };
        self.engine.stop_sandbox(id, deadline).await
    }
}

// Task API — delegates to engine guest operations
impl TaskService for K8sAdapter {
    async fn create(&self, req: CreateTaskRequest) -> Result<CreateTaskResponse> {
        let info = self.engine.create_container(&req.id, req.into()).await?;
        Ok(CreateTaskResponse { pid: info.pid })
    }
    async fn exec(&self, req: ExecProcessRequest) -> Result<()> {
        self.engine.exec_process(&req.sandbox_id, &req.container_id,
                                  req.into()).await?;
        Ok(())
    }
    async fn kill(&self, req: KillRequest) -> Result<()> {
        self.engine.kill_process(&req.sandbox_id, &req.container_id,
                                 req.pid, req.signal).await
    }
    async fn wait(&self, req: WaitRequest) -> Result<WaitResponse> {
        let exit = self.engine.wait_process(&req.sandbox_id,
                       &req.container_id, req.pid).await?;
        Ok(WaitResponse { exit_status: exit.code, exited_at: exit.timestamp })
    }
}
```

The adapter code is identical for both runtime modes — the `GuestRuntime` implementation determines actual behavior.

### Direct Adapter: Native Sandbox API

```protobuf
service SandboxService {
    rpc CreateSandbox(CreateSandboxRequest) returns (Sandbox);
    rpc StartSandbox(StartSandboxRequest) returns (StartSandboxResponse);
    rpc StopSandbox(StopSandboxRequest) returns (google.protobuf.Empty);
    rpc DeleteSandbox(DeleteSandboxRequest) returns (google.protobuf.Empty);
    rpc GetSandbox(GetSandboxRequest) returns (Sandbox);
    rpc ListSandboxes(ListSandboxesRequest) returns (ListSandboxesResponse);
    rpc WatchSandbox(WatchSandboxRequest) returns (stream SandboxEvent);

    // Runtime Snapshot Lifecycle (appliance mode)
    rpc PauseSandbox(PauseSandboxRequest) returns (PauseSandboxResponse);
    rpc ResumeSandbox(ResumeSandboxRequest) returns (ResumeSandboxResponse);
    rpc SnapshotSandbox(SnapshotSandboxRequest) returns (SnapshotSandboxResponse);
}

message StartSandboxRequest {
    string sandbox_id = 1;
    StartMode start_mode = 2;        // AUTO, RESTORE_EAGER, BOOT
    optional string snapshot_ref = 3;
    optional string image_ref = 4;
    uint64 ready_timeout_ms = 5;
    LifecyclePhase lifecycle_phase = 6; // FirstLaunch, UserPauseResume, BMSMigration, NodeDrain
}

message StartSandboxResponse {
    string sandbox_id = 1;
    uint64 ready_ms = 2;          // Total time to READY
    uint64 restore_ms = 3;        // VMM restore duration
    uint64 resume_ms = 4;         // VMM resume duration
    StartMode mode_used = 5;      // Actual mode (may degrade from AUTO)
    string snapshot_type = 6;     // "template" | "runtime" (which snapshot was used)
}

enum LifecyclePhase {
    FIRST_LAUNCH = 0;
    USER_PAUSE_RESUME = 1;
    BMS_MIGRATION = 2;
    NODE_DRAIN = 3;
}

message PauseSandboxRequest {
    string sandbox_id = 1;
    PauseMode pause_mode = 2;              // SNAPSHOT or SUSPEND
    bool snapshot_chunk_enabled = 3;       // chunk-ify runtime snapshot and upload
    uint64 drain_timeout_ms = 4;           // best-effort request drain before pause
    LifecyclePhase lifecycle_phase = 5;
}

enum PauseMode {
    SNAPSHOT = 0;   // Pause + create runtime snapshot (persistent, supports cross-node resume)
    SUSPEND = 1;    // Pause only (in-memory, same-node resume only)
}

message PauseSandboxResponse {
    string sandbox_id = 1;
    optional string snapshot_ref = 2;       // runtime snapshot reference (if snapshot mode)
    string chunk_upload_status = 3;         // "completed" | "partial" | "failed" | "skipped"
    uint64 chunks_total = 4;
    uint64 chunks_uploaded = 5;
    uint64 chunks_deduped = 6;
}

message ResumeSandboxRequest {
    string sandbox_id = 1;
    optional string snapshot_ref = 2;       // None = resume from in-memory paused state
    LifecyclePhase lifecycle_phase = 3;
    uint64 restore_timeout_ms = 4;
    uint64 ready_timeout_ms = 5;
}

message ResumeSandboxResponse {
    string sandbox_id = 1;
    uint64 ready_ms = 2;
    StartMode mode_used = 3;
    string snapshot_type = 4;               // "runtime"
    LifecyclePhase lifecycle_phase = 5;
}

message SnapshotSandboxRequest {
    string sandbox_id = 1;
    string output_dir = 2;
    bool chunk_enabled = 3;
}

message SnapshotSandboxResponse {
    string snapshot_ref = 1;
    string meta_path = 2;
    optional string chunk_upload_status = 3;
}
```

The Direct Adapter exposes no Task/Container concepts and provides richer response data (timing breakdowns, degradation status) that the K8s Adapter cannot surface through the containerd event model.

**K8s Adapter does NOT expose Pause/Resume/Snapshot**: The containerd `Sandboxer` trait has no `Pause`/`Resume`/`Snapshot` methods. Runtime snapshot lifecycle is only available through the Direct Adapter. This is by design — K8s workloads use pod eviction and rescheduling for lifecycle management, not VM-level snapshot/restore.

### Appliance Protocol Specification

**Transport**: vsock (Linux) or hvsock (Hyper-V). Guest listens on a configurable port (default: 1024).

**Encoding**: JSON Lines (one JSON object per line). Chosen for simplicity and debuggability; can be upgraded to a binary protocol later without changing semantics.

**Guest readiness contract**:
- PID1 (or an init script) sets up the application environment and starts the application.
- Once the application can serve requests, PID1 sends `{"type":"READY","sandbox_id":"..."}` on the vsock connection.
- On receiving `{"type":"SHUTDOWN","deadline_ms":30000}`, PID1 must initiate graceful shutdown within the specified deadline.
- If PID1 encounters an unrecoverable error, it sends `{"type":"FATAL","sandbox_id":"...","reason":"..."}`.

**Host behavior**:
- After VMM resume, the host connects to the vsock port and waits for `READY`.
- If `READY` is not received within `ready_timeout_ms`, the host kills the VMM process and reports `READY_TIMEOUT`.
- The host may send `PING` for connectivity checks and `CONFIG` for one-time configuration injection (e.g., DNS, hostname, environment variables).

### Configuration

```toml
# kuasar.toml

[engine]
runtime_mode = "appliance"   # "appliance" | "standard"
vmm_type = "cloud-hypervisor" # "cloud-hypervisor" | "firecracker"

[adapter]
type = "direct"              # "direct" | "k8s"
listen = "unix:///run/kuasar/engine.sock"

[appliance]
ready_timeout_ms = 5000
vsock_port = 1024
shutdown_timeout_ms = 30000

[admission]
max_concurrent_starts = 10
restore_timeout_ms = 3000

[cloud-hypervisor]
binary = "/usr/bin/cloud-hypervisor"

[firecracker]
binary = "/usr/bin/firecracker"
```

### Observability and Events

The engine emits structured events at each lifecycle stage:

**Start lifecycle (`sandbox.start.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.start.requested` | Entry | Start request received by engine |
| `sandbox.start.admission_passed` | Admission | Admission check passed |
| `sandbox.start.restore_begin` | VMM | Snapshot restore initiated |
| `sandbox.start.restore_end` | VMM | Snapshot restore completed |
| `sandbox.start.resume` | VMM | VMM resume completed |
| `sandbox.start.ready` | Guest | Guest reported READY |
| `sandbox.start.ready_timeout` | Guest | READY timeout (failure path) |
| `sandbox.start.degraded` | Engine | Start mode degraded (e.g., restore → boot) |
| `sandbox.start.rejected` | Admission | Start rejected (e.g., `NODE_RETIRED`) |

**Stop lifecycle (`sandbox.stop.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.stop.requested` | Entry | Stop request received |
| `sandbox.stop.shutdown_sent` | Guest | SHUTDOWN message sent to guest |
| `sandbox.stop.completed` | Exit | VM process exited |

**Pause lifecycle (`sandbox.pause.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.pause.requested` | Entry | Pause request received (tag: `lifecycle_phase`) |
| `sandbox.pause.drain_begin` | Drain | Request drain started |
| `sandbox.pause.drain_end` | Drain | Request drain completed or timed out |
| `sandbox.pause.vm_pause_begin` | VMM | VMM pause initiated |
| `sandbox.pause.vm_pause_end` | VMM | VMM pause completed |
| `sandbox.pause.completed` | Exit | Pause operation completed |

**Runtime snapshot (`sandbox.snapshot.runtime.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.snapshot.runtime.begin` | VMM | CH snapshot creation started |
| `sandbox.snapshot.runtime.end` | VMM | CH snapshot creation completed |
| `sandbox.snapshot.runtime.chunk_begin` | Chunk | Chunk-ify started |
| `sandbox.snapshot.runtime.chunk_end` | Chunk | Chunk-ify completed |
| `sandbox.snapshot.runtime.chunk_failed` | Chunk | Chunk-ify failed (degraded to full file) |
| `sandbox.snapshot.runtime.batch_exists_begin` | Chunk | BatchExists query started |
| `sandbox.snapshot.runtime.batch_exists_end` | Chunk | BatchExists query completed |
| `sandbox.snapshot.runtime.upload_begin` | Chunk | Chunk upload started |
| `sandbox.snapshot.runtime.upload_end` | Chunk | Chunk upload completed |
| `sandbox.snapshot.runtime.upload_failed` | Chunk | Chunk upload failed |

Runtime snapshot events also include: `chunks_total`, `chunks_deduped`, `chunks_uploaded`.

**Resume lifecycle (`sandbox.resume.*`)**:

| Event | Stage | Description |
|-------|-------|-------------|
| `sandbox.resume.requested` | Entry | Resume request received (tag: `lifecycle_phase`, `snapshot_type`) |
| `sandbox.resume.restore_begin` | VMM | Runtime snapshot restore started |
| `sandbox.resume.restore_end` | VMM | Runtime snapshot restore completed |
| `sandbox.resume.vm_resume` | VMM | VMM resume completed |
| `sandbox.resume.ready` | Guest | Guest reported READY after resume |
| `sandbox.resume.ready_timeout` | Guest | READY timeout after resume |
| `sandbox.resume.restore_failed` | VMM | Runtime snapshot restore failed |

Each event includes: `sandbox_id`, `timestamp`, `duration_ms` (where applicable), `vmm_type`, `runtime_mode`, `lifecycle_phase`.

### Boot Mode Selection: Benchmark-Informed Heuristics

Snapshot restore is **not always faster** than cold boot. Benchmarks on CH v50.0 (`/bench/cold_boot_vs_snapshot_restore.md`) reveal a critical crossover:

| Workload | Cold Boot | Snapshot Restore | Faster Mode |
|----------|-----------|-----------------|-------------|
| nginx-slim (260 MB working set, 4 GB allocated) | **499 ms** | 1,520 ms | Cold boot (3× faster) |
| nginx-fat (3.5 GB working set, 4 GB allocated) | 2,045 ms | **1,831 ms** | Snapshot restore (marginal) |

**Root cause**: CH v50.0's `fill_saved_regions()` reads the **entire** `memory-ranges` file synchronously (~1.4s warm, ~1.8s cold for 4 GB), regardless of how much guest memory is actually used. For lightweight workloads where application startup is faster than this memory read, cold boot wins.

**Two-dimensional decision**: Boot mode selection is `f(workload_type, lifecycle_phase)`, not just `f(workload_type)`. The lifecycle phase determines snapshot type and decision rules:

| Lifecycle Phase | Snapshot Type | Decision Rule |
|----------------|---------------|---------------|
| `FirstLaunch` | Template snapshot (if available) | Use crossover heuristic below |
| `UserPauseResume` | Runtime snapshot (required) | Always `SnapshotRestore` — must restore in-flight state |
| `BMSMigration` | Runtime snapshot (cross-node) | Always `SnapshotRestore` — must restore in-flight state |
| `NodeDrain` | Runtime snapshot (cross-node) | Always `SnapshotRestore` — must restore in-flight state |

For `FirstLaunch`, the caller applies the crossover heuristic:

```
if snapshot_available
   AND snapshot_template_compatible
   AND estimated_app_startup_cost > estimated_snapshot_restore_cost:
    request StartMode::RestoreEager
else:
    request StartMode::Boot
```

Where `estimated_snapshot_restore_cost` ≈ 1.4–1.8s for a 4 GB VM on current CH v50.0 (dominated by synchronous memory read). This crossover point drops dramatically with future CH optimizations (mmap-based lazy restore would reduce slim restore to ~50 ms).

For `UserPauseResume` / `BMSMigration` / `NodeDrain`, the heuristic does not apply — the VM must resume from its runtime snapshot to preserve application state. If the runtime snapshot is unavailable, the error `RUNTIME_SNAPSHOT_NOT_FOUND` is returned.

**Kuasar's role**: Kuasar itself does **not** auto-select the optimal mode. The caller specifies `StartMode` and `LifecyclePhase` in the request. When `StartMode::Auto` is used for `FirstLaunch`, Kuasar attempts restore if a compatible template exists and falls back to boot otherwise. For non-FirstLaunch phases, `StartMode::Auto` always resolves to `SnapshotRestore` using the runtime snapshot.

**Future optimization**: Once CH implements `mmap(MAP_PRIVATE)` for `memory-ranges` (demand-paged lazy restore), snapshot restore is expected to drop to ~50 ms for lightweight workloads (only faulting in used pages). At that point, the decision rule simplifies to: **always use SnapshotRestore when a compatible snapshot is available**.

See [`/docs/design/boot_mode_selection_strategy.md`](/docs/design/boot_mode_selection_strategy.md) for the full two-dimensional decision matrix and detailed analysis.

### Artifact-to-Start-Mode Mapping

Each `StartMode` requires specific artifacts from kuasar-builder. The following table maps start modes to their required inputs and disk setup:

| Start Mode | Required Artifacts | Disk Setup | Memory Setup |
|---|---|---|---|
| `Boot` | `rootfs.ext4` + kernel + initrd (Image Product Phase 1) | `rootfs.ext4` passed as virtio-blk disk | Standard `--memory size=NM` |
| `Boot` + lazy disk | `blockmap/manifest/chunkstore` + kernel + initrd (Image Product Phase 1 with `--emit-chunks`) | block-agent FUSE mount → virtio-blk | Standard `--memory size=NM` |
| `RestoreEager` | CH snapshot bundle + `overlay.qcow2` (Snapshot Product Phase 2) | Disk config from snapshot's `config.json`; only overlay needs host-side preparation | Memory from `memory-ranges` file (CH reads it synchronously) |
| `RestoreEager` + lazy disk | CH snapshot bundle + `base-disk/` chunkstore + `memory-disk/` chunkstore (Snapshot Product Phase 2 with block-agent) | block-agent mounts base via FUSE; overlay.qcow2 layered on top; snapshot `config.json` rewritten to point at FUSE path | Memory via block-agent FUSE mount of `memory-disk/` chunks (memory-zone file-backed) |
| `Auto` | Both Image Product and Snapshot Product available | Engine tries restore path first; falls back to boot path on template incompatibility or absence | Per resolved mode |

**Key implementation detail for restore mode**: In `RestoreEager`, the VMM's `restore()` implementation must **not** pass `--kernel`, `--memory`, `--cpus`, or `--disk` CLI flags — CH reads all configuration from the snapshot's `config.json`. Only `--restore source_url=file://<path>` and `--api-socket` should be passed. See [CH v50.0 Implementation Constraints](#ch-v500-implementation-constraints).

**Key implementation detail for boot mode**: In `Boot`, `add_disk()` must be called before `boot()`. The `DiskConfig` may point to either a raw `rootfs.ext4` file or a FUSE-mounted file from block-agent, depending on whether lazy disk loading is configured.

### RootfsProvider Trait: Pluggable Disk Backend

The `RootfsProvider` trait decouples Kuasar from any specific disk image format or content delivery mechanism. Two implementations are provided:

```rust
/// Provides a disk image file for VM boot or restore.
/// The returned path is passed to Vmm::add_disk().
trait RootfsProvider: Send + Sync {
    /// Prepare a disk image. For boot mode, this creates/mounts the rootfs.
    /// For restore mode, this prepares the base disk (if lazy loading is used).
    /// Returns the file path to be used as the virtio-blk backing file.
    async fn prepare(&self, req: RootfsPrepareRequest) -> Result<DiskPath>;

    /// Release resources (unmount FUSE, stop block-agent) after VM exit.
    async fn release(&self, sandbox_id: &str) -> Result<()>;
}

struct RootfsPrepareRequest {
    sandbox_id: String,
    image_ref: String,
    cache_domain: String,
    /// If set, prepare for restore mode (base disk + overlay).
    /// If None, prepare for boot mode (raw rootfs).
    snapshot_ref: Option<String>,
}

/// Returns either a direct file path or a FUSE mount path.
enum DiskPath {
    /// Direct file path (e.g., /path/to/rootfs.ext4).
    Direct(PathBuf),
    /// FUSE mount path — caller must keep the provider alive until release().
    FuseMount { path: PathBuf, overlay: Option<PathBuf> },
}
```

**LocalRootfsProvider** (simple, for development and lightweight deployments):
- `prepare()`: Returns the path to `rootfs.ext4` directly. No lazy loading.
- `release()`: No-op.

**BlockAgentRootfsProvider** (production, for lazy disk loading):
- `prepare()`: Starts an `artifactd` instance (if not already running), starts a `block-agent` process with the blockmap, mounts the FUSE filesystem, and returns the FUSE file path.
- `release()`: Unmounts FUSE, stops block-agent process, cleans up cache directory.

This separation ensures that Kuasar's core engine has no dependency on FUSE, artifactd, or the chunked image format. The `RootfsProvider` is injected at process startup alongside the VMM and GuestRuntime backends.

### Snapshot Template Version Validation

Before attempting snapshot restore, the engine validates the `snapshot.meta.json` against the current node environment. This prevents restore failures from version mismatches and ensures predictable degradation.

**Validated fields** (from `snapshot.meta.json`):

| Field | Validation | On Mismatch |
|-------|-----------|-------------|
| `ch_version` | Must match the installed CH binary version | Degrade to `Boot` |
| `guest.kernel` | Digest must match the configured kernel | Degrade to `Boot` |
| `guest.initrd` | Digest must match the configured initrd | Degrade to `Boot` |
| `compat.requires` | All required devices (e.g., `virtio-blk`, `vsock`) must be available | Degrade to `Boot` |

```rust
impl TemplateManager {
    /// Validate that a snapshot template is compatible with the current
    /// node environment. Returns Ok(()) if restore can proceed, or
    /// Err with a descriptive reason for degradation.
    fn validate_compat(&self, snap: &SnapshotRef, vmm: &dyn Vmm) -> Result<()> {
        let meta = self.load_meta(snap)?;

        // CH version must match exactly (CH does not guarantee cross-version compat)
        if meta.ch_version != self.node_ch_version {
            return Err(Error::CompatMismatch(format!(
                "ch_version: snapshot={} node={}",
                meta.ch_version, self.node_ch_version
            )));
        }

        // Kernel and initrd digests must match
        if meta.guest.kernel != self.node_kernel_digest {
            return Err(Error::CompatMismatch("kernel digest mismatch".into()));
        }
        if meta.guest.initrd != self.node_initrd_digest {
            return Err(Error::CompatMismatch("initrd digest mismatch".into()));
        }

        // Required device capabilities
        let caps = vmm.capabilities();
        for req in &meta.compat.requires {
            match req.as_str() {
                "virtio-blk" => {} // always available
                "vsock" => {}      // always available
                "vfio" if !caps.vfio => {
                    return Err(Error::CompatMismatch("vfio required but unavailable".into()));
                }
                _ => {} // unknown requirements are ignored (forward compat)
            }
        }

        Ok(())
    }
}
```

**Version domain strategy**: During rolling upgrades, new CH versions produce new snapshot templates in a new version domain. Old templates remain valid until the old CH version is fully decommissioned. The scheduler should prefer nodes whose version domain matches the template's `ch_version` (cache-aware scheduling). Mismatched nodes degrade to cold boot with an observable `sandbox.start.degraded` event.

---

## Compatibility

### Backward Compatibility

- **Standard mode (K8s Adapter + VmmTaskRuntime)** preserves the exact same behavior as current Kuasar.
- The existing `Sandboxer` trait, ttrpc client, and Task API code paths are unchanged — they are merely relocated into the K8s Adapter and VmmTaskRuntime modules.
- Existing Cloud Hypervisor sandbox configurations continue to work.
- No breaking changes to the public API surface.

### K8s and containerd Compatibility

In standard mode with K8s Adapter:
- Full shimv2 compatibility: all existing containerd/CRI-O integrations work unchanged.
- Full Task API: `Create`/`Start`/`Exec`/`Kill`/`Wait`/`Stats`/`Delete` all function as before.
- containerd events (`TaskCreate`, `TaskStart`, `TaskExit`, `TaskOOM`) are published normally.

In appliance mode with K8s Adapter (optional, for minimal K8s integration):
- `Create`/`Start` work normally (create and start the sandbox/VM).
- `Exec` returns `UNIMPLEMENTED` (documented, expected for appliance workloads).
- `Attach` returns `UNIMPLEMENTED`.
- `Kill` maps to `SHUTDOWN` message.
- `Wait` returns the VM exit code.
- `Stats` reads host-side cgroups.

### VMM Compatibility Matrix

| Operation | Cloud Hypervisor | Firecracker |
|-----------|-----------------|-------------|
| Cold boot | ✅ | ✅ |
| Snapshot restore | ✅ (`--restore`) | ✅ (`PUT /snapshot/load`) |
| Resume | ✅ (`PUT /vm.resume`) | ✅ (`PATCH /vm`) |
| Pause + Snapshot | ✅ | ✅ (`PUT /snapshot/create`) |
| Vsock | ✅ | ✅ |
| virtio-blk | ✅ | ✅ |
| Hot-plug disk | ✅ | ❌ (configure at creation) |
| VM resize (cpu/mem) | ✅ | ❌ |
| PMEM/DAX | ✅ | ❌ |
| Diff snapshot | ❌ | ✅ |

The engine gracefully handles capability differences via `VmmCapabilities`. Features unavailable on a given VMM are either skipped (with a logged warning) or trigger a documented degradation path.

### CH v50.0 Implementation Constraints

The following CH v50.0 behaviors have been identified through benchmarking ([`/bench/snapshot_restore_slim_vs_fat.md`](/bench/snapshot_restore_slim_vs_fat.md), [`/bench/snapshot_restore_256m_vs_4g.md`](/bench/snapshot_restore_256m_vs_4g.md)) and directly affect the `CloudHypervisorVmm` implementation:

#### Constraint 1: `--kernel` flag silently disables `--restore`

When both `--kernel` and `--restore` are provided on the CH command line, CH prioritizes `--kernel` and performs a fresh boot, **silently ignoring** `--restore`:

```
// CH v50.0 internal dispatch:
if payload_present {        // ← --kernel triggers this
    VmCreate + VmBoot       // fresh boot, --restore IGNORED
} else if restore_params {
    VmRestore               // only reachable without --kernel
}
```

**Mandatory implementation rule**: `CloudHypervisorVmm::restore()` must **only** pass `--restore source_url=file://<path>` and `--api-socket <path>`. It must **not** pass `--kernel`, `--memory`, `--cpus`, `--disk`, or any other VM configuration flags. All configuration is read from the snapshot's `config.json`.

This was the root cause of invalid benchmark results that reported ~26 ms "restore" times — they were actually measuring cold boot.

#### Constraint 2: Synchronous `fill_saved_regions()` bottleneck

CH v50.0 reads the **entire** `memory-ranges` file via sequential `read()` calls during restore, regardless of guest memory utilization:

```
Restore path:
  vm_restore() → MemoryManager::new_from_snapshot()
    → fill_saved_regions(memory_ranges_file, ranges)
      → for each range: read_volatile_from(file, guest_memory)  // full 4GB sequential read
```

Measured performance for a 4 GB VM:
- **Warm page cache**: ~1.39s (2.9 GB/s, memory bandwidth limited)
- **Cold page cache**: ~1.83s (adds ~0.44s disk I/O at ~9.3 GB/s NVMe)

There is no zero-page detection, no sparse loading, and no demand-paging. A 4 GB VM with only 169 MB of non-zero data (4.1% density) takes the same time to restore as one with 3.8 GB (91.8% density).

**Impact on design**: This is why cold boot (499 ms) beats snapshot restore (1,520 ms) for lightweight workloads. The boot mode selection heuristic described in [Boot Mode Selection](#boot-mode-selection-benchmark-informed-heuristics) accounts for this.

**Future mitigation**: An `mmap(MAP_PRIVATE)` approach for the `memory-ranges` file would enable demand-paged restore (~50 ms for slim workloads). This requires a CH upstream patch and is tracked as a future optimization.

#### Constraint 3: Memory-zone file-backed restore is broken

CH v50.0 does not correctly restore VMs created with `--memory-zone file=<path>`:
- Snapshot records zone configuration correctly in `config.json` and `state.json`.
- The `memory-ranges` file is empty (the file **is** the memory).
- But restore creates 512 MB default memory, never opens the zone file.
- VM runs with uninitialized memory (broken state).

**Mandatory workaround**: Use standard `--memory size=NM,shared=on` for snapshot creation. Do not use `--memory-zone file=` until CH fixes the restore path. The `kuasar-builder` snapshot pipeline must use this workaround.

**Impact on memory template strategy**: The file-backed memory template approach (L2 in the memory subsystem design) requires this fix. Until then, the memory file is produced as a byproduct of the standard `--memory` snapshot and chunked into `memory-disk/` for storage/distribution. At restore time, CH reads it synchronously via `fill_saved_regions()` rather than demand-paging via mmap.

#### Constraint 4: Snapshot serial path must be writable

The snapshot's `config.json` records the serial output file path used during snapshot creation. On restore, CH opens this path for writing. The restore host must ensure the directory exists and is writable, or rewrite the path in `config.json` before restore.

---

## Implementation Plan

### Milestones

**M1: Core Architecture Refactoring**

- Extract `Vmm` trait and `CloudHypervisorVmm` implementation from existing code.
- Create `SandboxEngine` with generics over `Vmm` and `GuestRuntime`.
- Implement `VmmTaskRuntime` by relocating existing ttrpc client code.
- Relocate existing `Sandboxer`/Task code into `K8sAdapter`.
- **Deliverable**: Existing tests pass with refactored code. No behavior change.

**M2: Appliance Mode**

- Implement `ApplianceRuntime` (vsock JSON Lines protocol).
- Implement `DirectAdapter` (gRPC server).
- Add appliance-specific configuration parsing.
- Add appliance integration tests (minimal rootfs VM, READY/SHUTDOWN).
- **Deliverable**: Appliance mode works end-to-end with Cloud Hypervisor.

**M3: Snapshot Restore Path**

- Add `restore()` + `resume()` to `CloudHypervisorVmm`, respecting CH v50.0 constraints (no `--kernel` with `--restore`; standard `--memory` mode only).
- Implement `TemplateManager` with `snapshot.meta.json` version validation and degradation.
- Implement `RootfsProvider` trait with `LocalRootfsProvider` and `BlockAgentRootfsProvider`.
- Implement start mode negotiation (auto/restore/boot with degradation based on version compatibility).
- Create `kuasar-builder` Phase 1 (Image Build: OCI → rootfs.ext4 + optional chunks) and Phase 2 (Snapshot Build: VM → snapshot bundle + base-disk + memory-disk).
- **Deliverable**: Both cold boot and snapshot restore paths work end-to-end; boot mode selection is caller-driven.

**M3.5: Runtime Snapshot Lifecycle**

- Add `snapshot()` to `Vmm` trait (CH: `PUT /vm.snapshot`; FC: `PUT /snapshot/create`).
- Implement `pause_sandbox()` / `resume_sandbox()` / `snapshot_sandbox()` in `SandboxEngine`.
- Implement request drain (best-effort, with `drain_timeout_ms`).
- Implement runtime snapshot metadata (`runtime_snapshot.meta.json`) with `snapshot_type`, `origin_boot_mode`, `sandbox_id`.
- Implement runtime snapshot chunker: memory dump → 512 KiB chunks → BatchExists (dedup) → PutChunk (upload to artifactd).
- Add `PauseSandbox` / `ResumeSandbox` / `SnapshotSandbox` RPCs to Direct Adapter protobuf.
- Add degradation path: chunk upload failure does not block pause; falls back to full file storage.
- **Deliverable**: Full pause/resume lifecycle works end-to-end; cross-node migration via chunk pipeline; chunk upload is optional and degradation-safe.

**M4: Firecracker VMM Support**

- Implement `FirecrackerVmm` (`Vmm` trait).
- Adapt `kuasar-builder` for Firecracker snapshot format.
- **Deliverable**: All four mode × VMM combinations work.

**M5: Admission Controller + Observability**

- Implement `AdmissionController` with concurrency and budget limits.
- Add structured event emission at all lifecycle stages.
- Add metrics export (Prometheus-compatible).
- **Deliverable**: Production-ready observability and resource governance.

### Code Changes Overview

```
kuasar/
├── vmm/
│   ├── engine/                    # NEW: Core engine (Layer 2)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── sandbox.rs         # SandboxEngine<V, R>
│   │       ├── admission.rs       # AdmissionController
│   │       ├── template.rs        # TemplateManager + version validation
│   │       ├── boot_mode.rs       # Boot mode selection helpers
│   │       ├── lifecycle.rs       # NEW: pause_sandbox / resume_sandbox / snapshot_sandbox
│   │       └── config.rs
│   │
│   ├── vmm-trait/                 # NEW: Vmm trait definition
│   │   └── src/lib.rs             # includes snapshot() method
│   │
│   ├── runtime-snapshot/          # NEW: Runtime snapshot pipeline
│   │   └── src/
│   │       ├── lib.rs             # Runtime snapshot orchestrator
│   │       ├── chunker.rs         # Memory dump → 512 KiB chunks
│   │       ├── uploader.rs        # BatchExists + PutChunk to artifactd
│   │       ├── metadata.rs        # runtime_snapshot.meta.json read/write
│   │       └── blockmap.rs        # Memory blockmap generation
│   │
│   ├── rootfs-provider/           # NEW: RootfsProvider trait + impls
│   │   └── src/
│   │       ├── lib.rs             # trait RootfsProvider
│   │       ├── local.rs           # LocalRootfsProvider (direct file)
│   │       └── block_agent.rs     # BlockAgentRootfsProvider (FUSE)
│   │
│   ├── cloud_hypervisor/          # REFACTORED from sandbox/src/cloud_hypervisor/
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for CloudHypervisorVmm (+ snapshot())
│   │       └── api.rs             # CH REST API client
│   │
│   ├── firecracker/               # NEW: Firecracker VMM backend
│   │   └── src/
│   │       ├── vmm.rs             # impl Vmm for FirecrackerVmm (+ snapshot())
│   │       └── api.rs             # FC REST API client
│   │
│   ├── runtime-vmm-task/          # REFACTORED from sandbox/src/client.rs
│   │   └── src/lib.rs             # impl GuestRuntime for VmmTaskRuntime
│   │
│   ├── runtime-appliance/         # NEW: Appliance guest runtime
│   │   └── src/
│   │       ├── lib.rs             # impl GuestRuntime for ApplianceRuntime
│   │       └── protocol.rs        # JSON Lines encode/decode
│   │
│   ├── adapter-k8s/               # REFACTORED from sandbox/src/sandbox.rs
│   │   └── src/
│   │       ├── sandboxer.rs       # impl Sandboxer
│   │       ├── task.rs            # impl TaskService
│   │       └── events.rs          # containerd event publishing
│   │
│   └── adapter-direct/            # NEW: Direct gRPC adapter
│       └── src/
│           ├── server.rs
│           └── proto/
│               └── sandbox.proto
│
├── kuasar-builder/                # NEW: Build pipeline subproject
│   ├── cmd/kuasar-builder/
│   ├── pkg/
│   │   ├── oci/
│   │   ├── rootfs/
│   │   ├── snapshot/
│   │   └── template/
│   └── Cargo.toml
│
└── cmd/
    └── kuasar-engine/             # NEW: Unified entry point
        └── main.rs
```

---

## Alternatives Considered

### Alternative 1: Appliance as a configuration flag within existing architecture

Add `if appliance_mode { ... }` checks throughout the existing code without architectural refactoring.

**Rejected**: Scatters mode-specific logic across the codebase, making each mode harder to test independently and increasing the risk of regressions. The `GuestRuntime` trait provides clean separation at the type level.

### Alternative 2: Separate binary for appliance mode

Build a completely separate `kuasar-appliance` binary that doesn't share code with the standard Kuasar.

**Rejected**: Leads to code duplication in VM management, device configuration, and lifecycle logic. The three-layer architecture achieves code sharing at Layer 1 (VMM) and Layer 2 (Engine) while allowing mode-specific behavior at Layer 1b (GuestRuntime) and Layer 3 (Adapter).

### Alternative 3: Dynamic per-sandbox mode selection at runtime

Allow each sandbox to independently choose standard or appliance mode during its lifecycle.

**Rejected**: Adds runtime complexity (dynamic dispatch, mixed-mode state management) for a use case that doesn't exist in practice — a Kuasar node serves one type of workload. Process-level mode selection is simpler and more predictable.

### Alternative 4: Build lazy image loading directly into Kuasar

Include chunked image loading, multi-tier caching, and FUSE block agent directly in Kuasar.

**Rejected**: Content delivery evolves independently of VM lifecycle management. Following the containerd/Nydus precedent, these concerns are better served by a separate project that Kuasar can optionally integrate with through a pluggable disk path interface.

### Alternative 5: Place runtime snapshot chunk upload in the external content system

Instead of Kuasar performing the chunk-ify + upload, have the external content system (artifactd/Miracle) watch for snapshot files and chunk-ify them externally.

**Rejected**: The chunk-ify trigger is inherently tied to the `pause_sandbox` lifecycle — only the engine knows *when* a VM is paused and *which* files constitute the runtime snapshot. If the content system had to detect and chunk-ify snapshots, it would need to understand VM lifecycle (which sandbox is pausing, when the snapshot file is fully written), violating the separation of concerns. Instead, Kuasar performs the chunking and HTTP uploads; the content system handles storage, dedup, distribution, and cross-node availability. The upload is optional and degradation-safe — if artifactd is unavailable, the snapshot file remains intact on local disk.

---

## References

1. **Kuasar Project**: https://github.com/kuasar-io/kuasar
2. **Cloud Hypervisor**: https://github.com/cloud-hypervisor/cloud-hypervisor
3. **Firecracker**: https://github.com/firecracker-microvm/firecracker
4. **On-demand Container Loading in AWS Lambda** (USENIX ATC '23): Marc Brooker et al. — content-addressed lazy loading for serverless containers.
5. **Modal.com Fast Container Loading**: https://modal.com/blog/serverless-containers — FUSE-based lazy loading for AI workloads.
6. **Nydus Image Service** (CNCF): https://nydus.dev — lazy container image loading, precedent for separating content delivery from container runtime.
7. **Kata Containers Architecture**: https://github.com/kata-containers/kata-containers/tree/main/docs/design/architecture — guest agent (`kata-agent`) + ttrpc model that appliance mode simplifies.
8. **TrENV** (SOSP '24): Repurposable sandbox pool and memory templates for fast VM startup.
9. **Cold Boot vs Snapshot Restore Benchmark**: [`/bench/cold_boot_vs_snapshot_restore.md`](/bench/cold_boot_vs_snapshot_restore.md) — measured crossover point between cold boot and snapshot restore on CH v50.0.
10. **Snapshot Restore Slim vs Fat Benchmark**: [`/bench/snapshot_restore_slim_vs_fat.md`](/bench/snapshot_restore_slim_vs_fat.md) — corrected restore measurements proving CH reads entire memory-ranges file synchronously; identified the `--kernel` + `--restore` bug.
11. **Build Pipeline Subsystem Design**: [`/docs/design/build_pipeline_subsystem_design.md`](/docs/design/build_pipeline_subsystem_design.md) — Image Product, Snapshot Product, and Runtime Snapshot Chunker specifications.
12. **Boot Mode Selection Strategy**: [`/docs/design/boot_mode_selection_strategy.md`](/docs/design/boot_mode_selection_strategy.md) — two-dimensional decision heuristics (`f(workload, lifecycle_phase)`) for cold boot vs snapshot restore.
13. **Memory Subsystem Design (CH)**: [`/docs/design/memory_subsystem_design_ch.md`](/docs/design/memory_subsystem_design_ch.md) — capability levels L1–L5 for memory restore optimization.
14. **artifactd Subsystem Design**: [`/docs/design/artifactd_subsystem_design.md`](/docs/design/artifactd_subsystem_design.md) — bidirectional chunk pipeline (read path + runtime snapshot upload path).
15. **Artifact Formats**: [`/docs/specs/artifact_formats.md`](/docs/specs/artifact_formats.md) — runtime snapshot metadata (`runtime_snapshot.meta.json`) and chunk storage layout.
16. **Interface Summary**: [`/docs/specs/interface_summary.md`](/docs/specs/interface_summary.md) — cross-document contract index including PauseSandbox/ResumeSandbox APIs and runtime snapshot events.
17. **Kuasar + Miracle Project Split**: [`/docs/design/project_split_kuasar_miracle.md`](/docs/design/project_split_kuasar_miracle.md) — runtime snapshot lifecycle integration between Kuasar (engine) and Miracle (content delivery).
