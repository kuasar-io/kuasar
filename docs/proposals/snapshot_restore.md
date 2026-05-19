# Snapshot and Restore

<!-- toc -->
- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [AI Agent Workload Design Matrix](#ai-agent-workload-design-matrix)
  - [Cloud-Native Workload Analysis](#cloud-native-workload-analysis)
  - [Phased Scope](#phased-scope)
  - [Design Overview](#design-overview)
    - [User Contract](#user-contract)
    - [CLI Contract](#cli-contract)
    - [API Contract](#api-contract)
  - [Risks and Mitigations](#risks-and-mitigations)
    - [Risk 1: Unclear Lifecycle Ownership](#risk-1-unclear-lifecycle-ownership)
    - [Risk 2: Partial Restore Failure](#risk-2-partial-restore-failure)
    - [Risk 3: Network State Is Not Reusable](#risk-3-network-state-is-not-reusable)
    - [Risk 4: Host/Guest State Mismatch on Restore](#risk-4-hostguest-state-mismatch-on-restore)
    - [Risk 5: Process Identity Divergence on Re-use](#risk-5-process-identity-divergence-on-re-use)
    - [Risk 6: Shared Snapshot With Running Processes](#risk-6-shared-snapshot-with-running-processes)
    - [Risk 7: Stale Template Key Matching](#risk-7-stale-template-key-matching)
    - [Risk 8: Node-Level Policy Is Too Coarse for Mixed Workloads](#risk-8-node-level-policy-is-too-coarse-for-mixed-workloads)
- [Design Details](#design-details)
  - [Lifecycle Model](#lifecycle-model)
  - [Metadata Layers](#metadata-layers)
  - [SnapshotGraph](#snapshotgraph)
  - [Sandbox Lifecycle Integration](#sandbox-lifecycle-integration)
  - [Snapshot Types](#snapshot-types)
  - [Template Pool](#template-pool)
  - [Snapshot Flow](#snapshot-flow)
  - [Restore Transaction Model](#restore-transaction-model)
  - [EnvironmentSnapshot Restore Path](#environmentsnapshot-restore-path)
  - [WarmForkSnapshot Restore Path](#warmforksnapshot-restore-path)
  - [ContinuationSnapshot Restore Path](#continuationsnapshot-restore-path)
  - [Memory Restore Modes and Resource Pinning](#memory-restore-modes-and-resource-pinning)
  - [Storage Handling](#storage-handling)
  - [Network Model](#network-model)
  - [Admin API](#admin-api)
  - [Hypervisor API Version Validation](#hypervisor-api-version-validation)
  - [Guest Agent Integration](#guest-agent-integration)
- [Implementation Details](#implementation-details)
- [Test Plan](#test-plan)
- [Production Readiness](#production-readiness)
- [Future Enhancements](#future-enhancements)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
<!-- /toc -->

## Summary

This document describes snapshot and restore support for Kuasar sandboxes. The long-term goal is to move Kuasar from cold-booting a VM for every sandbox toward a snapshot-native OCI runtime, but the implementation must first make lifecycle and consistency semantics correct.

The design is split into phases instead of treating the full Template Pool, Admin API, WarmForkSnapshot, and ContinuationSnapshot as one initial deliverable:

1. **Phase 1: EnvironmentSnapshot**. Validate the hypervisor restore process model and integrate it into the existing `KuasarSandboxer::start` state machine. No containers in the template; restore applies vsock, sandbox files, and cgroups, then hotplugs a fresh network namespace per instance. Delivers the complete EnvironmentSnapshot closure including network.
2. **Phase 2: Template Pool and Admin API**. Add template leases, GC, metrics, refill, and management APIs once Phase 1 restore is correct.
3. **Phase 3: WarmForkSnapshot**. Process in ready-waiting state; task injection before process resumes. Each restored instance gets an independent guest memory copy.
4. **Phase 4: ContinuationSnapshot**. Full process state with network identity; exclusive 1:1 consume. Cross-node continuation is only valid once distributed fencing and block-layer transfer backends exist. The first operational scope is same-node restart recovery or shared-filesystem migration; true cross-node migration remains gated on those prerequisites.

The primary rule is: **restore correctness is more important than early performance optimization**. Any snapshot file, memory backend, block device, or metadata object referenced by a running sandbox must remain pinned and must not be released or garbage-collected by the pool.

**Snapshot purpose is declared immutably at creation time** and determines snapshot content, restore operation, and sharing semantics. Three types cover the full range of AI agent and cloud-native workloads:

1. **EnvironmentSnapshot** — a VM with dependencies pre-installed but no running containers. One read-only snapshot shared across unlimited concurrent instances via shared lease (with Phase 2 Template Pool). Targets stateless task-parallel workloads (SWE agents, FaaS, CI jobs, code-execution sandboxes). Network is hotplugged after restore; each instance gets a fresh independent network namespace.
2. **WarmForkSnapshot** — a snapshot of a process in ready-waiting state (model loaded, runtime warm). Each restore starts from the shared template snapshot and injects new task identity before execution resumes. Requires the workload to implement the ready-waiting protocol. Standard service-mesh sidecars that cannot also enter ready-waiting are out of scope until per-container restore policy lands.
3. **ContinuationSnapshot** — full process state with network identity (virtual IP, container ID, in-memory state), for migrating long-running agents or stateful services without restart. Exclusive: consumed on restore; cannot be shared. Cross-node migration additionally requires distributed fencing and block-layer ownership transfer; until those land, the practical scope is same-node or shared-filesystem restore.

There is no restore-time mode switch. The type alone drives all restore behavior.

## Motivation

### Goals

1. Reduce sandbox startup latency through pre-warmed EnvironmentSnapshot templates.
2. Define a snapshot lifecycle that can be restored, rolled back, and garbage-collected safely.
3. Integrate restore with the existing sandbox create/start/stop/delete/recover paths.
4. Separate runtime snapshot metadata, immutable image metadata, and sandbox runtime metadata.
5. Support virtio-blk backed container snapshots through copyable, reflinkable, and pinnable block images.
6. Reserve abstractions for working set restore, lazy paging, and execution-image-style runtime architecture.
7. Support pod-level restore strategy annotation so different workloads on the same node can independently select restore mode, memory backend, and template key.

### Non-Goals

1. Cross-hypervisor snapshot portability.
2. TCP-transparent live migration between hosts (preserving existing network sessions / TCP connections across node migration).
3. Restoring TCP connections, conntrack, vsock connections, or live network sessions in the initial phase.
4. Pulling images, unpacking images, or independently creating OCI bundles inside the VMM sandboxer.
5. Container snapshots with the virtiofs backend in the initial design.
6. Exposing orphan containers to containerd before host-side state has been recovered.
7. Incremental snapshots, compression, encryption, or remote memory backends in the initial phase.
8. Process warm-fork with pre-restore identity injection (fork-on-restore pattern for inference agents). Phase 3 WarmForkSnapshot addresses warm-fork via post-restore task injection instead.
9. Transparent process continuation with preserved network identity and CRIU TCP repair across node migration. Phase 4 ContinuationSnapshot addresses migration but existing TCP connections are not preserved.

## Proposal

### User Stories

#### Story 1: Fast Sandbox Startup for High-Density Workloads

Operators want to avoid cold-booting a VM for every short-lived pod sandbox. An EnvironmentSnapshot template captures a booted VM with the guest agent ready and dependencies pre-installed but no running containers. After restore, the runtime hotplugs the network, applies config files, and calls `setup_sandbox()`.

#### Story 2: Managed Template Pool

Operators need visibility into template depth, hit rate, restore latency, active leases, pinned bytes, and disk usage. Phase 2 exposes these through an Admin API.

#### Story 3: Workload Warmup

In Phase 3, selected workloads may restore from WarmForkSnapshot templates. The process must be in ready-waiting state at snapshot time. The template key binds process binary digest, runtime version, and ready-state protocol version so a stale or incompatible snapshot cannot be restored.

#### Story 4: AI Agent Fast Startup

SWE agents, code-execution sandboxes, and other task-parallel workloads require many identical VM environments that start with a clean state for each task. An EnvironmentSnapshot template captures a pre-warmed VM (language runtime installed, repository cloned, build cache warm) with no running containers. Each new task restores from this shared read-only template in under one second and receives a freshly hotplugged independent network namespace. The template is never consumed; hundreds of concurrent agents share the same snapshot.

#### Story 5: Inference Agent Warm Fork

An inference service loads a multi-gigabyte model into memory once. A snapshot is taken after the model is loaded. Each new inference task restores from this snapshot: the forked process starts from the snapshot memory (with loaded model weights), and the new task identity (task ID, input parameters) is injected into process environment before the process begins executing. This pattern is covered by Phase 3.

#### Story 6: Long-Running Agent Continuation

A planning agent accumulates hours of context—file edits, tool outputs, conversation history—across a long-running session. When the host node is evicted or drained, the agent must resume on another node without restarting. Full checkpoint/restore with network identity preservation is required: the process retains its in-memory state and virtual IP, though existing TCP connections break on node migration and must be re-established. This pattern is covered by Phase 4.

### AI Agent Workload Design Matrix

This matrix maps AI agent workload types to the required snapshot target, restore semantics, and the gap between current mechanisms and the target design. It serves as the requirements anchor for future enhancements.

| Agent Type | Snapshot Target | Restore Semantics | Snapshot Type & Phase |
|---|---|---|---|
| **SWE Agent** (code generation / repair) | VM environment: OS + language runtime + dependencies + repository clone. No running containers. | Create fresh containers after restore. Inject task parameters via environment variables. Network rebuilt via hotplug. Full environment isolation per task. | **`EnvironmentSnapshot`** (Phase 1–2). Phase 1 introduces the restore path with network hotplug. Phase 2 introduces shared read-only template serving hundreds of concurrent instances via Template Pool. |
| **Inference Agent** (RAG / model inference) | Warm process memory: model weights loaded, inference runtime initialized, process in ready-waiting state. | Restore from shared template snapshot; inject new task ID / prompt / context via guest agent task-injection API before process PC advances. Independent network namespace and identity per instance. | **`WarmForkSnapshot`** (Phase 3). Process must implement the ready-waiting protocol. Phase 3 introduces task injection and independent restores. |
| **Long-Running Planning Agent** (interactive session / multi-hour task) | Complete process state + network identity: conversation history, file edits, tool outputs, in-progress plans. | Preserve process memory and Pod identity (IP, container ID) across migration. Existing TCP connections break on node move; agent must tolerate reconnect errors or use retry-safe tool calls. Supports migration to a new node via network identity transfer. | **`ContinuationSnapshot`** (Phase 4). Exclusive (1:1 consume). Original virtual IP routed or attached to the new node by external network identity controller. No IP refresh, no container ID rename. Existing network sessions not preserved. |

#### Summary Principles

**Snapshot purpose is declared at creation time, not at restore time.** The snapshot type alone determines what the snapshot contains and what restore operation is valid. There is no restore-time mode switch. Each type has exactly one restore path.

**Workload-to-type mapping:**

- SWE Agent / FaaS / CI runner → `EnvironmentSnapshot`
- Inference Agent / heavy-init runtime → `WarmForkSnapshot`
- Long-running Agent / stateful service migration → `ContinuationSnapshot`

**Sharing policy by snapshot type:**

- `EnvironmentSnapshot` templates are freely shareable. One read-only snapshot can serve unlimited concurrent instances, each with an independently hotplugged network namespace.
- `WarmForkSnapshot` templates use a shared lease; the template snapshot is read-only and reused across instances. Each instance gets an independent guest memory copy and an independent network namespace.
- `ContinuationSnapshot` is Exclusive. A captured process has a unique identity; sharing it across multiple Pods would violate process isolation.

**Network isolation per snapshot type:**

- `EnvironmentSnapshot` restore: hotplug a fresh network namespace after restore. Hard requirement; no fallback to a shared network path.
- `WarmForkSnapshot` restore: hotplug an independent network namespace for each fork instance before task injection. This only applies if every process in the Pod can satisfy the quiescent contract or a future per-container restore policy is available; standard service-mesh sidecars usually force a fallback to `EnvironmentSnapshot`.
- `ContinuationSnapshot` restore: preserve original virtual IP via network identity transfer (external network identity controller). No network reconfiguration inside the guest.

**Explicit interface contracts:**

- Snapshot content and restore semantics correspond one-to-one. An `EnvironmentSnapshot` template must contain no running processes. A `WarmForkSnapshot` template must contain one or more processes in ready-waiting state. A `ContinuationSnapshot` captures the process as-is.
- Each snapshot type defines a clear process handling contract at restore time: `EnvironmentSnapshot` has no processes to manage; `WarmForkSnapshot` injects new task identity before the process advances; `ContinuationSnapshot` restores the exact captured process.

### Cloud-Native Workload Analysis

This section maps common cloud-native workload categories to snapshot types and analyzes the design considerations that distinguish them from the AI agent scenarios. The goal is to establish that the three-type model is general-purpose, not AI-specific.

#### Workload Category Overview

| Category | Examples | Snapshot Type | Key Rationale |
|---|---|---|---|
| Stateless microservices | HTTP API, gRPC services, REST endpoints | `Environment` | High pod churn; clean state per instance; service mesh rebuilds routing on startup |
| Serverless / FaaS | Event handlers, webhook processors, scheduled jobs | `Environment` | Sub-second cold-start target; complete isolation per invocation |
| Batch / CI jobs | Build runners, test jobs, ETL pipelines | `Environment` | Pre-installed toolchain shared; source injected after restore |
| Warm-start runtimes | JVM services (Spring Boot), Python heavy-import workloads | `WarmFork` | Class loading / module import amortized across instances; stateless request handling |
| In-memory serving caches | Pre-loaded lookup tables, warm feature stores | `WarmFork` | Cache rebuild is expensive; each instance serves independent read-only requests |
| Stateful services | Databases (MySQL sidecar), message broker coordinators | `Continuation` | WAL position, buffer pool, connection pool state must survive node migration |
| Long-running user sessions | Jupyter notebooks, IDE backends, interactive REPLs | `Continuation` | User-accumulated kernel variables and file state must survive node eviction |
| Monitoring / logging agents | Fluentd, Prometheus node exporter | `Environment` | Stateless; all metrics written to external storage |
| Service mesh sidecars | Envoy, Istio proxy | `Environment` | Routing config and circuit-breaker state rebuilt from control plane on startup |

#### Stateless Microservices and FaaS

The correct snapshot type for a stateless service depends on **where the startup cost lies**:

| Startup profile | Bottleneck | Correct snapshot type |
|---|---|---|
| Thin handler (Go/Rust/Node), <1 s app init | VM boot (~3–5 s) | `EnvironmentSnapshot` |
| FaaS event handler, webhook processor | VM boot | `EnvironmentSnapshot` |
| JVM service (Spring Boot), Python heavy-import | App init: JVM + Spring context + connection pool (10–30 s) | `WarmForkSnapshot` |

**`EnvironmentSnapshot` scenario (FaaS / lightweight handlers)**:

A serverless platform runs Go event handlers. Each handler starts in < 200 ms once the VM is running, but VM cold-boot takes 3–4 s. With `EnvironmentSnapshot`, the VM is already booted; restore takes < 500 ms including network hotplug. The handler is ready in under 1 second total. The template is shared read-only across all concurrent invocations.

`EnvironmentSnapshot` does **not** accelerate application-level initialization (JVM warmup, Spring context loading, model loading). It only eliminates VM boot and OS/toolchain setup time. If a service's dominant startup cost is application-level initialization, use `WarmForkSnapshot` instead.

**`WarmForkSnapshot` scenario (JVM/Spring Boot services)**:

A deployment of 200 Spring Boot HTTP API replicas must scale from 50 to 200 instances during a traffic spike. Each replica has a 15-second cold-start: JVM initialization, Spring context loading, connection pool warming. Without restore, 150 new pods × 15 s = 2,250 pod-seconds of unserved traffic.

With `WarmForkSnapshot`, one warm Spring Boot process (ready-waiting after `ApplicationContext` is loaded and probes are passing) is snapshotted. Each new replica restores from this shared template snapshot; the JVM and Spring context are restored from snapshot state without re-running initialization. Replica startup drops to under 1 second.

Two restore modes are available:
- **Injection mode** (`kuasar.io/task-id` present): per-instance identity (service ID, connection pool config) is delivered via the injection protocol before the process starts accepting requests.
- **Autonomous mode** (`kuasar.io/task-id` absent): the process self-starts after receiving COMMIT, determining its own identity from environment variables or external configuration.

**Adaptation requirement**: Spring Boot services must implement the ready-waiting protocol. The recommended path is an init shim that calls `SpringApplication.run()`, waits for `ApplicationReadyEvent`, shuts down internal worker threads, closes all outbound connections (reaching quiescent state), then opens a Unix domain inject socket and blocks until COMMIT is received before admitting requests. Application logic is unchanged.

**Template key lifecycle**: the template key binds the application image digest and runtime version. A rolling deploy to a new image version invalidates the old template; the pool triggers a re-snapshot cycle using `pool-refill`. Old templates remain available until their in-flight restores drain.

#### Batch and CI Jobs

**Scenario**: A CI platform runs 500 concurrent build jobs per hour. Setup per job: Go toolchain, Docker daemon, dependency cache populated (45 seconds). Actual build: 3–8 minutes. With `EnvironmentSnapshot` (toolchain template), setup time drops to ~5 seconds (only the per-job repository clone remains). The toolchain template is shared read-only across all 500 concurrent jobs.

**Design separation**: the template captures the build environment (toolchain, caches), not the source code or job parameters. Source is injected after restore via environment variables and a post-restore git clone step. This mirrors the SWE Agent pattern but applies to general CI workloads.

**Template invalidation**: when the toolchain version changes (Go 1.22 → 1.23, Node 20 → 22), the template key changes automatically because the key includes the image digest. Old templates are GC'd after their leases expire.

#### Warm-Start Runtimes

**Scenario**: A Python service imports TensorFlow, NumPy, and loads a 200 MB configuration object at startup. Import time: 20 seconds. After initialization, the service handles stateless HTTP requests. During deploys, 50 new replicas must start simultaneously.

`WarmForkSnapshot` amortizes the 20-second import across all instances. Each instance restores from the shared template snapshot with modules and configuration already initialized; the import never re-runs. Each instance loads an independent guest memory copy.

**Adaptation requirement**: existing HTTP services do not naturally implement the ready-waiting protocol. Two adaptation paths are available:

1. **Init shim** (preferred): a thin wrapper that completes all initialization, reaches quiescent state (stops all threads, closes all outbound connections), then opens a Unix domain inject socket and blocks until task injection completes before starting the HTTP server. Application logic is unchanged; only the entry point is wrapped.

2. **EnvironmentSnapshot fallback**: for services that cannot satisfy the Quiescent Contract (active connections, non-idempotent background threads, or mid-execution state at quiesce time), `EnvironmentSnapshot` is the correct fallback — the 20-second startup cost is paid, but correctness is guaranteed.

#### Stateful Services

**Scenario**: A MySQL sidecar runs alongside a stateless API pod. It maintains an active connection pool, write-ahead log position, and in-memory buffer pool (512 MB). A node is drained for maintenance; all pods must migrate within 5 minutes.

- `EnvironmentSnapshot` is **not applicable**: MySQL would lose its WAL position and buffer pool; recovery requires checkpoint replay, which is slow and risks data loss.
- `WarmForkSnapshot` is **not applicable**: MySQL is not a task-parallel workload; task injection makes no semantic sense.
- `ContinuationSnapshot` is the correct type: the process resumes with its exact WAL position, buffer pool state, and in-memory data structures. The original virtual IP is routed to the new node. **Existing TCP connections break** on node migration (the endpoint moves to a new physical host); clients must reconnect to the same virtual IP. In-flight requests are lost and must be retried. If the workload protocol supports reconnect (e.g. MySQL client reconnect, gRPC retry), service resumes without data loss once the client reconnects. True TCP-transparent migration requires CRIU TCP repair integration and is deferred to a future enhancement.

**Durable state requirement**: `ContinuationSnapshot` migrates process memory but not disk files. The WAL and data files must be accessible from the target node. Options:
- Shared distributed block storage (Ceph RBD, cloud EBS with multi-attach).
- `MovedExclusive` block migration: the `RestoredBlockArtifacts::MovedExclusive` action transfers exclusive block layer ownership to the new sandbox directory. **Local `rename(2)` is only valid when source and destination are on the same node or a shared filesystem path**. Cross-node migration requires one of: (a) shared distributed block storage (Ceph RBD, cloud EBS) that the target node re-attaches; (b) a distributed block device with lease transfer and fencing to prevent split-brain; or (c) async copy with cutover: copy data to the target node first, then atomically cut over ownership once complete. The `MovedExclusive` action is an abstract ownership transfer; the concrete backend must provide fencing guarantees. Cross-node distributed fencing is a future enhancement; see `StorageContinuationBackend` in Future Enhancements.

#### Multi-Container Pods and Sidecar Coordination

A pod typically includes a main container and one or more sidecars (service mesh proxy, logging agent, secrets injector). The pod sandbox (VM) has a single `SnapshotType`; all containers within the sandbox are governed by the same type.

| Pod `SnapshotType` | Main container behavior | Sidecar behavior |
|---|---|---|
| `Environment` | Fresh start; task parameters via env | Fresh start; routing state rebuilt from control plane |
| `WarmFork` | Warm-forked; task injected | Warm-forked from same VM snapshot — sidecar must **also be in ready-waiting state** at snapshot time |
| `Continuation` | Transparently resumed | Transparently resumed (circuit-breaker counters, cached routes all preserved) |

**Sidecar hard constraint for `WarmFork`**: all processes in the VM — main container and every sidecar — are forked from the same snapshot. The safety invariant of `WarmForkSnapshot` is that **no process has yet executed with any identity at the time of snapshot**. Allowing a sidecar to resume mid-execution breaks this invariant: the sidecar already holds stale connection pool state, credential caches, xDS routes, or lease tokens from the snapshot-time pod's identity. This is the same identity-chimera problem that `WarmForkSnapshot` was designed to eliminate.

Hard rule: **every process in a `WarmForkSnapshot` pod must be in ready-waiting state at snapshot time**. Sidecars that cannot implement ready-waiting (e.g. Envoy with open xDS connections, a secrets-injector with in-memory lease state) must either:
1. Be redesigned to start fresh after restore (fresh-start sidecar pattern: sidecar exits on `SIGTERM`, restarts clean after restore).
2. Cause the pod to be excluded from `WarmForkSnapshot` and use `EnvironmentSnapshot` instead.

"Resume mid-execution for stateless proxies" is not an acceptable exception — Envoy's xDS subscription and certificate rotation state is not safe to fork mid-execution. The correct design for a service-mesh sidecar is fresh-start after `EnvironmentSnapshot`, or a sidecar that natively implements the ready-waiting protocol. Per-container restore policy (allowing some containers to fresh-start while others warm-fork) is a future enhancement listed in Future Enhancements.

**When pod-level type is wrong for a sidecar**: if a stateful sidecar (e.g., a credential-rotation daemon with in-memory lease state) is included in a pod using `EnvironmentSnapshot`, the sidecar's in-memory state is discarded on every restore. The sidecar must re-acquire its state from an external source on startup — a standard design requirement that applies equally to cold boots.

#### Snapshot Type Selection Guide

The following decision tree applies to both AI agent and non-AI agent workloads:

```
Does the workload carry process-internal state that must survive
across pod instances or node changes?
│
├─ NO (stateless): use EnvironmentSnapshot
│   │
│   └─ Does initialization take more than ~5s (module import,
│      JIT, cache build) AND the workload handles independent
│      concurrent requests?
│       │
│       ├─ YES and can adopt ready-waiting protocol: use WarmForkSnapshot
│       └─ NO or cannot adapt: use EnvironmentSnapshot
│
└─ YES (stateful): use ContinuationSnapshot
    │
    └─ Is durable state (disk, WAL) accessible from the target node?
        ├─ YES (shared storage): ContinuationSnapshot directly
        └─ NO: ContinuationSnapshot + MovedExclusive block migration
```

### Phased Scope

#### Phase 1: EnvironmentSnapshot

- The template VM contains no containers.
- The template VM contains no sandbox-specific network devices.
- Restore reapplies vsock, console, sandbox files, and cgroups.
- Network hotplug: tap/macvtap devices are hotplugged after VM restore; guest network namespace is configured before `setup_sandbox()`. Network hotplug is a **hard requirement** for `EnvironmentSnapshot` — there is no fallback to a no-network path.
- Phase 1 targets a single pre-provisioned template to validate the restore path. Shared-lease pool machinery enabling unlimited concurrent instances and per-instance CoW block layer is introduced in Phase 2.
- On miss or restore failure, policy can fall back to fresh boot and must clean all prepared resources.
- `environment` is the default infrastructure path; no explicit snapshot-type or identity annotation is required.
- `EnvironmentSnapshot` is the primary fast-startup mechanism for stateless task-parallel workloads.

#### Phase 2: Template Pool

- Shared templates use leases; they are not popped from the available queue and pushed back later.
- Exclusive templates use atomic consume.
- GC only removes templates with no active lease and no running-sandbox pin.
- `filebackend` and `ondemand` memory modes pin snapshot files until sandbox stop/delete.

#### Phase 3: WarmForkSnapshot

- Snapshot captures a process in "ready-waiting" state: the application has completed expensive initialization (model loaded, runtime warm), reached quiescent state (worker threads stopped, outbound connections closed), and is blocking on a Unix domain inject socket waiting for a COMMIT signal.
- virtio-blk only. Each restored instance gets an independent guest memory copy.
- Restore prepares an independent memory backend for the instance, hotplugs a fresh independent network namespace, reseeds guest entropy, then drives the injection protocol: **injection mode** (task-id present) delivers task parameters via PREPARE/READY/COMMIT/STARTED; **autonomous mode** (task-id absent) sends COMMIT directly and the process self-starts.
- Key validation: process binary digest + runtime version + ready-state protocol version + block image digest.
- No identity chimera. Task injection is a first-class designed API.
- Snapshot pod declares `kuasar.io/snapshot-type: warm-fork` and `kuasar.io/template-key=<key>`.
- Restore pod declares `kuasar.io/snapshot-type: warm-fork` and at least one of `kuasar.io/template-key=<key>` or `kuasar.io/template-id=<id>` to select the template. When both are set, both must match simultaneously.
- `kuasar.io/memory-restore-mode` is a restore-time policy and does not belong on the snapshot pod.

#### Phase 4: ContinuationSnapshot

- Captures complete process state with network identity (virtual IP, container ID).
- Exclusive: 1:1 restore, consumed on use. Never shared.
- Process memory and virtual identity are preserved: no container ID rename, no IP refresh, no change to any in-memory state. The process resumes from the exact point it was interrupted. **Existing network sessions (TCP connections, open sockets) are not preserved**; workloads must tolerate reconnect errors on existing sockets after restore unless CRIU TCP repair is added.
- Requires **network identity transfer**: the original virtual IP / Pod IP must be routed or attached to the target node before the process resumes. This capability is provided by an external network identity controller — a network infrastructure component outside Kuasar whose concrete implementation may be an overlay network controller, VPC CNI secondary-IP reassignment, ENI/IP-alias migration, route-table update, or eBPF endpoint remapping. Kubernetes default semantics assign a new IP on pod rebuild; `ContinuationSnapshot` explicitly overrides this by requiring the external network identity controller to participate in the migration.
- Stored per-workload in a separate continuation store, not in the shared template pool.
- Restore pods declare `kuasar.io/snapshot-type: continuation`.
- `kuasar.io/pod-uid=<uid>` and `kuasar.io/workload-generation=<n>` identify the workload instance for restore selection; the snapshot is created from the running workload that already carries that identity in its runtime environment.

### Design Overview

The snapshot type is declared immutably as part of the workload contract and uniquely determines what the snapshot contains and what restore operation is valid. Restore pods carry the selector annotations needed to resolve the correct snapshot without guessing. `memory-restore-mode` is orthogonal: it changes how restore loads memory, not which snapshot semantics apply.

| Type | Snapshot contains | Restore operation | Sharing |
|---|---|---|---|
| `EnvironmentSnapshot` | OS + dependencies, no containers | Hotplug network → `setup_sandbox()` → create fresh containers | Unlimited shared (read-only) |
| `WarmForkSnapshot` | OS + warm process in ready-waiting state | Prepare memory backend → hotplug network → reseed entropy → drive injection protocol (injection or autonomous mode) | Shared lease; each instance has independent guest memory |
| `ContinuationSnapshot` | OS + full process state + network identity | Preserve process memory and virtual identity; transfer original IP to target node via external network identity controller; existing sessions not preserved | Exclusive (1:1, consumed) |

Phase 1 introduces `EnvironmentSnapshot`. Phase 2 introduces the Template Pool and Admin API. Phase 3 introduces `WarmForkSnapshot`. Phase 4 introduces `ContinuationSnapshot`.

```text
Snapshot Creation
  create VM
  wait guest agent
  quiesce guest
  pause VM
  capture memory/state/config/block refs
  resume or stop VM
  register RuntimeSnapshot

Restore
  acquire snapshot lease
  prepare memory backend
  prepare block backend
  patch sandbox-specific config
  start the hypervisor restore process
  verify guest agent
  attach or restore network
  setup sandbox
  commit sandbox state
  keep lease pinned as required by restore mode
```

### Risks and Mitigations

#### Risk 1: Unclear Lifecycle Ownership

Snapshots involve memory files, block images, metadata, template pool state, sandbox instances, and GC.

**Mitigations**: define lifecycle ownership, leases, pins, and SnapshotGraph dependencies. GC is driven by active leases and pins.

#### Risk 2: Partial Restore Failure

Memory could restore while a block device is missing, or config could be patched while the VM fails to start.

**Mitigations**: restore is transactional. Running status is not published before commit.

#### Risk 3: Network State Is Not Reusable

Tap fds and paths from a template cannot be reused across sandboxes.

**Mitigations**: `EnvironmentSnapshot` and `WarmForkSnapshot` templates do not contain sandbox-specific network devices. The runtime hotplugs a fresh network namespace after restore per instance. Live network sessions are out of scope.

#### Risk 4: Host/Guest State Mismatch on Restore

If a snapshot contains running container processes that the host and containerd do not know about, resuming those processes without restoring host-side state produces an inconsistent sandbox: containerd has no record of the containers, yet they are running in the guest.

**Resolution**: eliminated by design in the `SnapshotType` model. `EnvironmentSnapshot` contains no processes — there is nothing to mismatch. `WarmForkSnapshot` requires the process to be in ready-waiting state; task injection supplies the new per-instance identity before the process advances. `ContinuationSnapshot` is exclusive and restores the exact snapshot-time host state together with the guest state — the host-side container records are part of the continuation store.

#### Risk 5: Process Identity Divergence on Re-use

If a snapshot of a running process is restored into a new sandbox without refreshing process-internal state—parsed environment variables, DNS resolution cache, in-memory endpoint tables, connection pools, credential caches—the resumed process runs under a new external identity at the kernel level but retains a snapshot-era world-view in memory. This identity chimera is especially dangerous for stateful services (database clients, service-mesh sidecars, credential rotation logic).

**Resolution**: eliminated by the `SnapshotType` model. `WarmForkSnapshot` injects new task identity before the process executes from the ready-waiting point—the process never holds stale identity because it has never yet executed with any identity. `ContinuationSnapshot` preserves identity completely—there is no divergence because nothing is refreshed; the same identity continues on the new node. The identity chimera cannot arise from either path.

#### Risk 6: Shared Snapshot With Running Processes

If a snapshot containing running processes is shared across multiple sandboxes, every restored instance inherits processes that carry the original Pod's identity. The result is multiple distinct Pods running processes whose in-memory state reflects a different Pod's world-view.

**Resolution**: eliminated by design. `EnvironmentSnapshot` contains no processes. `WarmForkSnapshot` is shareable only because each fork starts fresh from the ready-waiting point with injected per-instance identity—there are no pre-existing process identities to share. `ContinuationSnapshot` is structurally exclusive and non-shareable. The problematic combination (shared snapshot with running identities) cannot be expressed.

#### Risk 7: Stale Template Key Matching

If the template key does not fully identify the snapshot content, a restore can silently use a snapshot that does not match the current pod configuration (different image digest, environment, seccomp profile, etc.).

**Resolution**: key construction is type-specific and validated at restore time. `EnvironmentSnapshot` key includes kernel path, image digest, vcpus, memory, kernel params, and storage backend. `WarmForkSnapshot` key includes process binary digest, runtime version, and ready-state protocol version. `ContinuationSnapshot` key is the exact workload identity (`pod_uid:generation`). Complete key validation (including PodSandboxConfig hash, CNI mode, sidecar set, seccomp/AppArmor profile) is a future enhancement; the initial implementation accepts a user-supplied opaque key string and relies on the operator to ensure correctness.

#### Risk 8: Node-Level Policy Is Too Coarse for Mixed Workloads

A global restore mode policy applies uniformly to all sandboxes on a node. A node running both stateless FaaS containers and stateful sidecar containers cannot express different policies for each.

**Resolution**: there is no global restore mode. Snapshot type (`kuasar.io/snapshot-type`) is a per-pod annotation. Different pods on the same node independently select `environment`, `warm-fork`, or `continuation`.

## Design Details

### Lifecycle Model

| Lifecycle | Object | Creator | Referenced by | Release/GC |
|---|---|---|---|---|
| Image | immutable block image | snapshotter/image service | snapshot, sandbox | image GC |
| RuntimeSnapshot | memory/state/config metadata | snapshot subsystem | template pool, sandbox lease | snapshot GC |
| MemoryBackend | memory-ranges, file backend, uffd backend | restore subsystem | running sandbox | sandbox stop/delete |
| BlockBackend | reflink/copy/moved block images | BlockProvider/restore | running sandbox | storage release |
| SandboxInstance | restored VM process and guest state | sandboxer | containerd | sandbox delete |
| RestoreLease | active snapshot reference | pool acquire | restore transaction/sandbox | release or sandbox stop |

Rules:

- copy memory mode may release part of the snapshot lease after restore commit, but audit metadata remains.
- filebackend, ondemand, and externaluffd must pin memory files or handlers until sandbox stop/delete.
- Block images referenced by symlink, file backend, or lazy backend must not be deleted by template GC.

### Metadata Layers

The design uses three metadata layers so snapshots do not own OCI data directly.

#### Layer 1: Immutable Image Metadata

- image digest, chunk mapping, verity metadata.
- owned by snapshotter or image service.
- referenced by RuntimeSnapshot.

#### Layer 2: Runtime Snapshot Metadata

- hypervisor state/config/memory references.
- device topology, block artifact references, memory restore policy.
- snapshot id, version, creation time, compatibility data.

#### Layer 3: Sandbox Runtime Metadata

- PodSandboxConfig, hostname, DNS, routes, interfaces.
- Kuasar storage metadata, id generator, container/process/IO state.
- cgroup, event forwarding, sandbox status.

### SnapshotGraph

Snapshots should not be managed as a plain `Vec<Snapshot>`. `SnapshotGraph` models dependencies between base snapshots, memory deltas, block deltas, immutable images, and running instances:

```rust
struct SnapshotGraph {
    nodes: HashMap<SnapshotNodeId, SnapshotNode>,
    edges: Vec<SnapshotEdge>,
}

enum SnapshotNodeKind {
    RuntimeSnapshot,
    MemoryBackend,
    BlockArtifact,
    ImmutableImage,
    SandboxInstance,
}
```

GC and restore ordering are computed from graph dependencies.

### Sandbox Lifecycle Integration

The normal CRI path selects restore explicitly:

1. `create()`: create host-side sandbox state and sandbox files, but do not start the VM.
2. `start()`:
   - run `hooks.pre_start()`.
   - prepare host-side network resources.
   - choose `FreshBoot` or `RestoreFromTemplate` based on config or pod annotation.
   - fall back to fresh boot on template miss or restore failure if policy allows.
   - after restore success, run `setup_sandbox()`, `hooks.post_start()`, `add_to_cgroup()`, and `dump()`.
3. `stop/delete()`: release sandbox leases, MemoryBackend, BlockBackend, and template references.
4. `recover()`: recover running sandbox leases and pins so the pool cannot GC files still in use.
   Background maintenance and GC tasks are started only after recovery completes.

Example policy:

```toml
[sandbox.snapshot]
enable_environment_restore = false
enable_warmfork_restore = false
max_concurrent_restores = 4
fallback_to_fresh_boot = true
default_memory_restore_mode = "copy"

[template_pool]
gc_interval_secs = 3600
```

There is no node-level restore mode default. Snapshot type is a per-pod declaration, not a global policy.

### User Contract

From a user perspective, the contract is split into three layers:

- `snapshot-type` decides the semantic mode.
- selector annotations locate the concrete snapshot or workload instance.
- `memory-restore-mode` chooses the restore strategy.

Protocol values are written in lowercase. Implementations may accept mixed case for compatibility, but clients should emit lowercase values.

`memory-restore-mode` is a restore-time policy only. It is exposed on `sandbox run` / restore APIs and is not required for snapshot creation.

| Field | Snapshot pod | Restore pod | Notes |
|---|---|---|---|
| `kuasar.io/snapshot-type` | no | warm-fork or continuation only | restore selector; `environment` is the default infrastructure path and does not need an explicit annotation |
| `kuasar.io/template-key` | no | warm-fork only | business key for template lookup; at least one of `template-key` or `template-id` is required |
| `kuasar.io/template-id` | no | warm-fork only | pin restore to a specific template by ID; can be used alone or combined with `template-key` (both must match when combined) |
| `kuasar.io/pod-uid` | no | continuation only | workload identity |
| `kuasar.io/workload-generation` | no | continuation only | workload identity version |
| `kuasar.io/warm-fork-ready-protocol-version` | warm-fork snapshot pod only | no | readiness gate checked before capture; restore pods do not repeat it |
| `kuasar.io/memory-restore-mode` | no | optional | `copy`, `ondemand`, `filebackend`, or `externaluffd`; restore-time only |

Recommended semantics:

- `environment` should remain fully automatic: no explicit snapshot-type annotation is required.
- `warm-fork` should require at least one of `template-key` or `template-id`; when both are set, both must match.
- `continuation` should require the workload identity pair.
- `memory-restore-mode` should never be used to infer snapshot type.

### CLI Contract

Recommended user-facing commands:

```text
kuasar-ctl template create
kuasar-ctl template list
kuasar-ctl template get --id <template-id>

kuasar-ctl sandbox run ...
kuasar-ctl sandbox list
kuasar-ctl sandbox get --id <sandbox-id>
kuasar-ctl sandbox destroy --id <sandbox-id>

kuasar-ctl pool status
kuasar-ctl pool refill
kuasar-ctl pool gc
```

Restore selectors should follow the same split:

- `environment`: no selector beyond the default infrastructure path
- `warm-fork`: `--template-key` and/or `--template-id`; at least one is required; when both are supplied, both must match simultaneously; `--snapshot-type` is required (no default)
- `continuation`: `pod-uid + workload-generation`
- `memory-restore-mode` may be supplied on `sandbox run` for any restore type; snapshot creation does not accept it as a primary selector.

### API Contract

Current on-wire JSON shape (admin socket protocol, flat fields):

```json
{
  "action": "sandbox-run",
  "snapshot_type": "warm_fork",
  "template_key": "nginx-default"
}
```

Or pin to a specific template ID (with optional key validation):

```json
{
  "action": "sandbox-run",
  "snapshot_type": "warm_fork",
  "template_id": "3f8a1c2d-...",
  "template_key": "nginx-default"
}
```

`template_key` and `template_id` may each be omitted; at least one is required. When both are set, the acquired template's key must match `template_key`.

For continuation:

```json
{
  "action": "sandbox-run",
  "snapshot_type": "continuation",
  "pod_uid": "5f5b6f61-...",
  "generation": 3
}
```

Recommended actions:

- `template-create`, `template-list`, `template-get`
- `sandbox-run`, `sandbox-list`, `sandbox-get`, `sandbox-destroy`
- `pool-status`, `pool-refill`, `pool-gc`

The user-facing contract above maps directly to the current on-wire actions for template management.

### Snapshot Types

Snapshot type is declared at creation time via the Admin API or pod annotation and is immutable. The type determines snapshot content, valid restore operations, and sharing semantics. There is no restore-time mode switch.

```rust
enum SnapshotType {
    Environment,    // no processes; freely shareable
    WarmFork,       // ready-waiting process; shared lease; each instance has independent guest memory
    Continuation,   // full process state; exclusive
}
```

#### Type 1: EnvironmentSnapshot

**Purpose**: provide a pre-warmed VM environment for stateless task-parallel workloads (SWE agents, FaaS, code-execution sandboxes). Each restored instance creates fresh containers with task parameters injected via environment variables.

Characteristics:

- No containers at snapshot time. No sandbox-specific network devices.
- Key: `env:{kernel_path}:{image_path}:{vcpus}:{memory_mb}:{kernel_params}:{storage_backend}:{guest_task_version}`
  - **Note**: guest task binary version is not yet included in the auto-computed key. It will be added once the sandboxer can reliably read a build-time constant from the guest image.
- Sharing: unlimited concurrent instances share the same read-only template via shared lease. Each instance gets a per-instance CoW block layer.
- Network: hotplug is a **hard requirement**. An `EnvironmentSnapshot` template is not eligible for any sandbox until network hotplug is available. There is no fallback to a no-network path.
- Restore: hotplug network → `setup_sandbox()` → containerd creates containers normally.
- Pool: shared lease model, auto-refill supported.

#### Type 2: WarmForkSnapshot

**Purpose**: share expensive initialized process state (e.g. loaded model weights) across many concurrent task instances without reloading. Each restored instance forks from the warm snapshot, receives a new task identity via injection, and executes independently.

Characteristics:

- Contains one or more processes in **ready-waiting state**: the application has completed expensive initialization, reached quiescent state, and is blocking on a Unix domain inject socket waiting for a COMMIT signal.
- virtio-blk only.
- Key: `fork:{process_binary_digest}:{runtime_version}:{ready_protocol_version}:{block_image_digest}`
- Sharing: shared template lease; the template snapshot is read-only and shared across instances. Each instance gets an independent guest memory copy and an independent block layer (reflink).
- Network: each fork gets a freshly hotplugged independent network namespace. Network hotplug is required.
- Restore: two modes determined by `kuasar.io/task-id` annotation on the restore pod:
  - **Injection mode** (task-id present): prepare memory → hotplug network → reseed entropy → run PREPARE/READY/COMMIT/STARTED via guest agent; task identity delivered before execution.
  - **Autonomous mode** (task-id absent or empty): prepare memory → hotplug network → reseed entropy → send COMMIT directly; process self-starts without an injected task identity.
- STARTED is the formal restore commit point in both modes.
- No identity chimera.
- Pool: shared lease model. Each acquired lease is an independent instance restored from the same template snapshot.

**Ready-waiting protocol** (overview):

```
[application startup]
  load model / warm runtime
  reach quiescent state (stop threads, close connections, flush I/O)
  bind Unix domain inject socket, block on accept()
    ← snapshot is taken here ←

  [on restore — injection mode]
    accept → CAPABILITIES → PREPARE (task_id, context, env_overrides)
    validate task parameters, send READY   ← no external side effects before this point
    [sandboxer barrier: waits for all targets to send READY]
    receive COMMIT → send STARTED → close connection
    apply env overrides, begin task execution with injected task_id

  [on restore — autonomous mode]
    accept → CAPABILITIES → receive COMMIT → send STARTED → close connection
    begin self-directed task execution (task_id is empty)
```

#### Type 3: ContinuationSnapshot

**Purpose**: migrate a long-running stateful process to another node without interruption. The process retains its complete identity—network address, container ID, environment, in-memory state—across the migration.

Characteristics:

- Contains complete process state and network identity (virtual IP, container ID, process memory).
- Sharing: **prohibited**. A continuation snapshot represents a unique running workload. Sharing would assign the same identity to multiple processes simultaneously.
- Exclusive: consumed atomically on restore. Each snapshot can be used exactly once.
- Key: workload identity = `{pod_uid}:{generation}`. Key is assigned by the workload owner, not auto-computed. Restore uses pod annotations `kuasar.io/pod-uid` and `kuasar.io/workload-generation`.
- Network: original virtual IP is preserved. The external network identity controller routes or attaches the original IP to the new node after restore. No IP refresh, no container ID rename.
- Restore: process memory and virtual identity are preserved exactly. **Existing network sessions (TCP connections, open sockets) are not preserved** — they break on node migration. Workloads must tolerate reconnect errors on existing sockets, or pair `ContinuationSnapshot` with CRIU TCP repair (deferred to a future enhancement).
- Storage: block layer ownership is transferred exclusively to the new sandbox directory via `MovedExclusive`. Cross-node transfer requires distributed block storage or fencing — see Stateful Services for backend options.
- Not in the shared template pool. Stored per-workload in a separate continuation store under `{store_dir}/continuation/{pod_uid}/`.

**Snapshot type summary**:

| Property | EnvironmentSnapshot | WarmForkSnapshot | ContinuationSnapshot |
|---|---|---|---|
| Containers at snapshot time | None | Yes (ready-waiting) | Yes (running) |
| Sharing | Unlimited | Shared lease; independent guest memory per instance | Prohibited |
| Network per instance | Freshly hotplugged | Freshly hotplugged | Preserved (original IP) |
| Container ID per instance | New | New | Preserved |
| Process memory identity | N/A (no process) | Injected at restore | Preserved exactly |
| Orphan management | None | None | None |
| Pool model | Shared lease (restore-scoped; released on commit for copy mode, else held until sandbox stop) | Shared lease (sandbox-scoped; held for full sandbox lifetime regardless of memory mode) | Exclusive acquire (1:1 consume); stored in independent `ContinuationStore`, not `TemplatePool` |
| Block layer | Reflink per instance | Reflink per instance | Move/rename (exclusive) |

### Template Pool

The pool is backed by a `HashMap<TemplateKey, VecDeque<PooledTemplate>>` with LIFO acquire order. Templates are persisted to disk and rehydrated after a sandboxer restart. Current disk layout:

- `{store_dir}/environment/{template_id}/` — `EnvironmentSnapshot` templates; `template_id` is a server-generated UUID string.
- `{store_dir}/warmfork/{template_id}/` — `WarmForkSnapshot` templates; `template_id` is a server-generated UUID string.

`ContinuationSnapshot` is managed by the independent `ContinuationStore` (not `TemplatePool`):

- `{store_dir}/continuation/{pod_uid}/g{generation}/` — `ContinuationSnapshot` entries

#### Shared Templates

Shared templates are not popped from the available queue. Acquiring one creates a lease:

```rust
pub async fn acquire_shared(&self, key: &TemplateKey) -> Result<TemplateLease> {
    // find latest template
    // increment active_lease_count
    // pin snapshot files according to restore mode
    // return lease
}
```

Release decrements the lease count. GC can remove only templates with `active_lease_count == 0` and no sandbox pin.

#### Exclusive Templates

Exclusive templates are consumed atomically:

```rust
pub async fn consume_exclusive(&self, id: &str) -> Result<OwnedTemplate> {
    // atomically mark consumed
    // remove from available index
    // transfer ownership to restoring sandbox
}
```

Exclusive disks must not be symlinked and then invalidated by deleting the template directory. They must be renamed/moved into the sandbox directory, or reflinked/hardlinked while preserving the real backing reference.

#### Consumed Marker

When an Exclusive template is acquired, a zero-byte `consumed` file is written to its directory. On sandboxer restart, any template directory with a `consumed` marker is skipped during pool rehydration so an already-consumed template is never re-inserted. The marker is removed by `release()` if the template is returned to the pool after a failed restore.

#### Concurrent Restore Limit

A `max_concurrent_restores` semaphore (default: 4, configurable via `[sandbox.snapshot] max_concurrent_restores`) limits the number of simultaneous restore operations across all snapshot kinds to cap host memory pressure when many sandboxes start at once.

### Snapshot Flow

Snapshot creation must quiesce the guest first:

1. guest agent runs pre-snapshot hooks.
2. supported filesystems run `sync` and optional `fsfreeze`.
3. target container processes are paused or frozen; active-I/O workloads require user hooks.
4. the hypervisor pauses the VM.
5. state, memory, and block references are captured.
6. the VM is resumed on success or failure, or moved to a diagnosable failed state.
7. RuntimeSnapshot metadata is written.
8. the template is registered or returned to the Admin API.

`drop_caches` is only an optimization and is not a consistency guarantee.

### Restore Transaction Model

Restore is a transaction, not a single call:

```text
prepare_snapshot
  validate metadata version and compatibility
  acquire template lease

prepare_memory_backend
  copy/reflink/pin memory files
  start uffd handler if needed

prepare_block_backend
  copy/reflink/move block images
  validate integrity and ownership

prepare_config
  patch vsock, console, disk paths, network policy

start_restore_process
  start the hypervisor in restore mode
  connect the hypervisor API socket

verify
  wait guest agent
  verify devices and mount state
  verify sandbox config can be applied

commit
  set sandbox Running
  dump sandbox state
  keep required leases pinned

rollback
  kill the hypervisor process
  detach devices
  release memory/block backends
  release template lease
  cleanup sandbox temp files
```

Success is not returned and Running status is not published before commit.

### EnvironmentSnapshot Restore Path

Phase 1. Template type: `Environment`.

Pre-restore validation:
- template key matches derived key from PodSandboxConfig (kernel, image, vcpus, memory, storage backend, guest task version).
- hypervisor API version in template matches current sandboxer's supported range.
- network hotplug capability confirmed; if unavailable, reject restore (no fallback to no-network path).

Steps:

1. Derive `EnvironmentSnapshot` key from config or pod annotation.
2. Acquire shared template lease (non-consuming; template remains in pool).
3. Prepare memory backend and hypervisor config.
4. Patch per-sandbox vsock, console, api socket, and paths (internal plumbing only; no network config at this stage).
5. Start hypervisor restore process; set `pids`, `wait_chan`, and client.
6. Wait for guest agent readiness.
7. Push sandbox files.
8. **Hotplug current tap/macvtap devices; run guest network setup** (hard requirement).
9. Call `setup_sandbox()`.
10. Run `hooks.post_start()`, `add_to_cgroup()`, and `dump()`.
11. Release or keep lease pinned based on memory mode.

### WarmForkSnapshot Restore Path

Phase 3. Template type: `WarmFork`.

Pre-restore validation:
- process binary digest matches template metadata.
- runtime version and ready-state protocol version are compatible.
- block image digest matches.
- network hotplug capability confirmed.

Steps:

1. Derive `WarmForkSnapshot` selector from pod annotations (`kuasar.io/snapshot-type: warm-fork`, `kuasar.io/template-key` and/or `kuasar.io/template-id`). When `template-id` is set, the template is acquired by ID; when only `template-key` is set, the latest template for that key is acquired. When both are set, the template is acquired by ID and its key is verified to match `template-key`.
2. Acquire shared template lease (template remains in pool; not consumed).
3. **Prepare memory backend**: create per-instance memory artifacts in the sandbox work directory according to the configured `memory_restore_mode` (see Memory Restore Modes). Each instance gets an independent guest memory copy.
4. Prepare block backend: reflink template block image into per-instance private directory.
5. Patch per-sandbox vsock, console, api socket (internal plumbing only).
6. Start hypervisor restore.
7. Wait for guest agent readiness.
8. **Hotplug a fresh independent network namespace** for this instance.
9. **Reseed guest entropy** (must happen before injection; each CoW fork instance must have independent entropy state).
10. **Run injection protocol** via guest agent `InjectTask` RPC (mode determined by `kuasar.io/task-id`):
    - **Injection mode** (task-id present): deliver `{task_id, context, env_overrides}` via PREPARE; await all targets' READY replies (pre-commit barrier) within `prepare_timeout_ms`; send COMMIT; await all targets' STARTED replies within `commit_timeout_ms`. If PREPARE times out, send CANCEL to already-READY targets and roll back.
    - **Autonomous mode** (task-id absent): send COMMIT directly (no PREPARE/READY); await all targets' STARTED replies within `commit_timeout_ms`.
    STARTED from all targets is the formal restore commit point in both modes.
11. Run `hooks.post_start()`, `add_to_cgroup()`, and `dump()`.

In injection mode, the process begins execution with the injected task identity. In autonomous mode, the process self-starts and determines its own identity.

### ContinuationSnapshot Restore Path

Phase 4. Template type: `Continuation`.

Pre-restore validation:
- workload identity (`pod_uid` + `generation`) matches exactly.
- no other instance of this workload is currently running (singleton enforcement).
- target node's external network identity controller can route or attach the original virtual IP / Pod IP.

Steps:

1. Derive workload identity from `kuasar.io/pod-uid` and `kuasar.io/workload-generation`, then locate the continuation snapshot in the per-workload continuation store.
2. **Consume atomically** (mark consumed; snapshot cannot be restored again).
3. Prepare memory backend (copy or filebackend; filebackend pins files until sandbox stop).
4. **Transfer block layer ownership** (`MovedExclusive`) into new sandbox directory. For same-node or shared-filesystem migration this is a local `rename(2)`. For cross-node migration the storage backend must provide distributed fencing and detach/attach semantics; see Stateful Services for backend options.
5. Patch per-sandbox vsock, console, and api socket (internal plumbing only).
6. **Transfer network identity**: instruct the external network identity controller to route or attach the original virtual IP / Pod IP to this node. Do not assign a new IP; do not modify any network identity in the snapshot.
7. Start hypervisor restore.
8. Wait for guest agent readiness.
9. Verify original network identity is reachable from the new node.
10. Commit. The process resumes from exactly the point it was interrupted. Process memory and virtual identity are fully preserved. **Existing TCP connections and open sockets are not preserved** — the process will observe reconnect errors or ECONNRESET on any sockets that were active at snapshot time. Workloads must handle this (retry, reconnect, or tolerate the error).

No `setup_sandbox()` call. No network reconfiguration inside the guest. No container ID rename. The process's complete in-memory state and virtual identity are preserved; network session state is not.

### Memory Restore Modes and Resource Pinning

Four memory restore modes are defined, all passed as `memory_restore_mode` to the Cloud Hypervisor `PUT /vm.restore` API. Kuasar serializes the selected mode and CH handles the implementation; Kuasar's responsibility is limited to resource pinning (keeping the template directory alive for non-copy modes).

**Orthogonality**: `memory_restore_mode` is infrastructure orthogonal to `SnapshotType`. All four modes are valid for all three snapshot types. For `Continuation`, "exclusive consume" means the pool entry cannot be reused — it does not mean the snapshot files are deleted. Files persist on disk until the sandbox is explicitly deleted (VM is stopped before `cleanup_consumed` removes the directory), so `filebackend` and `externaluffd` backing stores are fully safe for Continuation.

**Important**: the four modes differ only in loading strategy (eager vs lazy), resource pinning lifetime, and who drives page fault handling.

**SnapshotType compatibility**:

| Mode | Environment | WarmFork | Continuation |
|---|---|---|---|
| copy | ✅ | ✅ | ✅ |
| ondemand | ✅ | ✅ | ✅ |
| filebackend | ✅ | ✅ | ✅ |
| externaluffd | ✅ | ✅ | ✅ |

```rust
enum MemoryRestoreMode {
    Copy,
    OnDemand,
    FileBackend,
    ExternalUffd,
}

trait MemoryBackend {
    async fn prepare(&self, snapshot: &RuntimeSnapshot) -> Result<PreparedMemory>;
    async fn prefetch(&self, policy: PrefetchPolicy) -> Result<()>;
    async fn release(&self, memory: PreparedMemory) -> Result<()>;
}
```

Mode semantics:

- **copy**: CH eagerly reads the full snapshot file into anonymous guest memory (MAP_PRIVATE memfd) before resuming the VM. After restore commit, the sandbox no longer references the snapshot file; the memory-file lease can be released immediately.
- **ondemand**: CH registers guest memory regions with an internal userfaultfd handler. Pages are faulted in from the snapshot file on first access. Snapshot files must remain pinned until sandbox stop/delete (the file is read on every uncached fault).
- **filebackend**: CH maps the snapshot memory file directly as file-backed guest memory (MAP_PRIVATE on `memory.img`). Multiple instances mapping the same file with MAP_PRIVATE share the OS page cache for clean pages, amortizing I/O; writes produce per-instance private copies at the kernel level. The snapshot file must remain pinned until sandbox stop/delete (it is the live backing store).
- **externaluffd**: CH registers guest memory regions with an external userfaultfd handler. The handler serves page faults from the snapshot file, enabling advanced behaviors such as working-set-aware prefetch and future KSM integration for asynchronous physical page merging. The snapshot file must remain pinned until sandbox stop/delete.

**Resource pinning rules by mode**:

| Mode | Snapshot file pinned after commit? | Who drives page faults? |
|---|---|---|
| copy | No — released after commit | CH (eager read at restore time) |
| ondemand | Yes — until sandbox stop/delete | CH internal uffd handler thread |
| filebackend | Yes — until sandbox stop/delete | Kernel (MAP_PRIVATE file-backed mmap) |
| externaluffd | Yes — until sandbox stop/delete | External uffd handler (CH-registered) |

Working set restore is a future optimization built on top of `ondemand` or `externaluffd`:

- hot pages: prefetched before resume.
- warm pages: prefetched in the background.
- cold pages: lazy fault.
- prefetch policy: derived from historical data or user config.

### Storage Handling

#### reflink and symlink boundaries

- symlink is only for immutable shared images, and the template directory must remain alive while any sandbox references it.
- reflink is used for snapshot delta or private COW copies from Shared templates.
- Exclusive mode transfers ownership and does not rely on "symlink then delete template directory".

#### virtio-blk integration

`WarmForkSnapshot` and `ContinuationSnapshot` require virtio-blk because virtiofs is tied to a specific virtiofsd/vhost-user socket. `EnvironmentSnapshot` also uses virtio-blk to allow per-instance CoW block layer reflinks. The current virtio-blk implementation provides copyable block artifacts and `DeviceGraph` storage identity tracking. A unified snapshot block provider and `SnapshotGraph`-driven restore transaction remain part of the snapshot/restore design target.

### Network Model

Initial support restores network configuration, not live network sessions:

- no TCP/vsock/conntrack restoration.
- `EnvironmentSnapshot` and `WarmForkSnapshot` templates do not contain sandbox-specific tap fds or paths; restore hotplugs fresh network devices and rebuilds the guest network namespace independently per instance.
- `ContinuationSnapshot` preserves the original virtual IP; the external network identity controller routes or attaches that IP to the new node. Existing TCP connections break (the endpoint moves to a new physical host); clients must reconnect to the same virtual IP. True TCP-transparent live migration requires additional CRIU TCP-repair integration and is deferred to a future enhancement.
- if network hotplug is not available, `EnvironmentSnapshot` and `WarmForkSnapshot` templates are not eligible for networked sandboxes.

### Admin API

The Admin API uses a Unix socket at `--admin-listen` (default `/run/vmm-sandboxer-admin.sock`) with `0600` permissions. Protocol: one JSON object per line, one request/response per connection.

The action names below reflect the current on-wire implementation. The user-facing contract above uses template/run terminology and maps directly to these actions.

Current implementation mapping:

- `template create` -> `template-create`
- `template list/get` -> `template-list` / `template-get`
- `sandbox run` -> `sandbox-run`
- `sandbox list/get/destroy` -> `sandbox-list` / `sandbox-get` / `sandbox-destroy`
- `pool status/refill/gc` -> `pool-status` / `pool-refill` / `pool-gc`
- `continuation list/delete` -> `continuation-list` / `continuation-delete`

#### Actions

**Sandbox actions:**

- `sandbox-run`: create a sandbox slot and restore from a template in one step. Accepts `template_id` (specific template), `template_key` (latest for pool key)`. The snapshot type is encoded in the template itself and is not a per-restore parameter. Returns `{"ok":true,"sandbox_id":"...","template_id":"..."}`.
- `sandbox-destroy`: stop a sandbox and release all resources. Works for both admin-created sandboxes and containerd-managed sandboxes that are stopped.
- `sandbox-list`: list all known sandboxes with status, base dir, template ID, and `snapshot_type`.
- `sandbox-get`: get details of a single sandbox including `snapshot_type`, `lease_mode`.

**Template actions:**

- `template-create`: snapshot a running sandbox and register it. Requires `sandbox_id` and `snapshot_type` (`"warm_fork"` or `"continuation"`); `key` is required for `warm_fork`, while `pod_uid` and optional `generation` identify `continuation`. `template_id` is generated by the sandboxer, and any request-supplied `template_id` is ignored. `"environment"` is not accepted — `EnvironmentSnapshot` templates are created by the pool's background refill loop, not via this call. For `warm_fork`, the sandbox process must already be in ready-waiting state before this call. Pods with `kuasar.io/template-key=<key>` will match this template at start time. For `"continuation"`, the entry is stored in `ContinuationStore` at `{store_dir}/continuation/{pod_uid}/g{generation}/` and does not enter the template pool.
- `template-list`: list available templates from the Environment/WarmFork pool plus available Continuation entries from the continuation store.
- `template-get`: get one available template by server-generated `template_id`.
- `template-from-bundle`: create a `WarmForkSnapshot` from an already-prepared OCI bundle/rootfs/storage metadata with the process in ready-waiting state.

**Pool actions:**

- `pool-status`: show templates, leases, pins, GC state, and metrics (hit rate, restore latency, per-type counts).
- `pool-refill`: trigger background `EnvironmentSnapshot` refill up to `target_depth`. Accepts `snapshot_type="environment"` (default). The current refill loop is single-key: it derives the Environment key from the active VM factory config for this sandboxer instance. `WarmForkSnapshot` and `ContinuationSnapshot` templates are managed manually via `template-create`.
- `pool-gc`: remove templates of the specified `snapshot_type` (`"environment"` or `"warm_fork"`) down to `target_depth`. `ContinuationSnapshot` entries are not subject to pool GC; use `continuation-delete` instead. Only removes templates with no active lease or pin.
- `continuation-list`: list all available (not yet consumed) continuation snapshot entries.
- `continuation-delete`: delete a specific continuation snapshot entry by `pod_uid` and `generation`.

Continuation entries also have a dedicated automatic GC path: consumed entries that are no longer
referenced by any live sandbox are removed by a background task that runs every
`template_pool.gc_interval_secs`. This is a condition-based cleanup path, not a TTL-based one.
Unconsumed entries are not automatically removed.

#### Concurrency rules

- lock order: sandbox map -> sandbox mutex -> template pool -> filesystem operation.
- every action validates sandbox status.
- template id/key allows only safe characters and no path separators.
- every error response includes action, template id, sandbox id, and phase.
- operations are audit logged.

### Hypervisor API Version Validation

Before implementation, verify the API contract of the target hypervisor:

- pause/snapshot/resume/restore API paths.
- request/response structs.
- restore memory mode field names and valid values.
- restore process model: start then call API, or start with source arguments.
- snapshot directory layout: state, memory-ranges, config, device state.

The hypervisor client wrapper must include phase and request summary in all errors. For the Cloud Hypervisor initial implementation, verify against the repository-pinned `api_client` commit and use `ChClient`.

### Guest Agent Integration

The guest agent provides:

- `Check`: verify agent readiness.
- `ExecVMProcess`: short-term path for sync, fsfreeze, and file writes.
- future structured RPCs:
  - `PrepareSnapshot`
  - `FreezeStorage`
  - `ThawStorage`
  - `WriteGuestFile`
  - `VerifyRestore`

On snapshot failure, guest state must be thawed/resumed or moved to a diagnosable failed state.

## Implementation Details

The types below reflect the Phase 4 design.

```rust
/// Snapshot type declared immutably at creation time.
/// Determines snapshot content, valid restore operations, and pool sharing semantics.
/// No restore-time mode switch exists; the type alone drives all restore behavior.
pub enum SnapshotType {
    /// No containers. Freely shareable. Requires network hotplug after restore.
    Environment,
    /// Ready-waiting process. Shared template lease; each instance has independent guest memory.
    /// Requires task injection after restore.
    WarmFork,
    /// Full process state + network identity. Exclusive (1:1 consume).
    /// Stored in `ContinuationStore` (independent of `TemplatePool`).
    Continuation,
}

/// Pool entry persisted as pooled_template.json in the template directory.
/// Replaces the RuntimeSnapshot design struct for Phase 1–2; RuntimeSnapshot
/// (with metadata_version, DeviceTopology, SandboxConstraints, BlockReference)
/// is the target for the SnapshotGraph phase.
pub struct PooledTemplate {
    pub id: String,
    pub key: TemplateKey,
    pub snapshot_dir: PathBuf,
    pub pmem_path: String,
    pub kernel_path: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub created_at_secs: u64,
    pub original_task_vsock: String,
    pub original_console_path: String,
    /// CH binary version recorded at snapshot time; used to detect cross-version restores.
    pub hypervisor_api_version: String,
    pub snapshot_type: SnapshotType,
    /// id_generator value of the snapshotted sandbox; restored sandboxes start here
    /// so hotplugged device IDs do not collide with IDs in the restored VM state.
    pub id_generator: u32,
    pub disk_images: Vec<DiskImageEntry>,
    pub storages: Vec<Storage>,
    /// Ready-waiting protocol version; validated before WarmFork restore.
    /// None for Environment and Continuation types.
    pub ready_protocol_version: Option<String>,
    /// Workload identity for Continuation snapshots: (pod_uid, generation).
    /// None for Environment and WarmFork types.
    pub workload_identity: Option<WorkloadIdentity>,
    /// Container IDs from the snapshotted sandbox that are no longer alive but whose
    /// storage must be re-attached at restore time (orphaned by sandbox lifecycle events).
    pub orphan_container_ids: Vec<String>,
    /// Snapshot sub-directories holding per-container CH snapshot state.
    pub snapshot_containers: Vec<String>,
}

/// Workload identity for ContinuationSnapshot. Uniquely identifies a specific
/// running workload instance; used to enforce singleton restore semantics.
pub struct WorkloadIdentity {
    pub pod_uid: String,
    pub generation: u32,
}

/// Three construction paths, one per snapshot type:
///   Environment:  TemplateKey::from_env_config(kernel, image, vcpus, memory_mb, kernel_params,
///                                               storage_backend, guest_task_version)
///                 Format: "env:{kernel_path}:{image_path}:{vcpus}:{memory_mb}:..."
///   WarmFork:     TemplateKey::from_fork_config(process_binary_digest, runtime_version,
///                                               ready_protocol_version, block_image_digest)
///                 Format: "fork:{process_binary_digest}:{runtime_version}:..."
///   Continuation: TemplateKey::from_workload_identity(pod_uid, generation)
///                 Format: "{pod_uid}:{generation}"
pub struct TemplateKey { pub key: String }

/// Active reference to a pooled template during a restore operation.
/// Dropped via complete() on success or fail() on failure; each kind releases
/// the appropriate pool resource.
pub(crate) struct TemplateLease {
    pool: Arc<TemplatePool>,
    tmpl: Option<PooledTemplate>,
    kind: RestoreLeaseKind,
}

pub(crate) enum RestoreLeaseKind {
    /// Environment: shared lease; template stays in pool; in_flight_restores decremented on drop.
    SharedLease,
    /// WarmFork Shared: shared reference lease; template stays in pool; in_use_count
    /// incremented on acquire, decremented on fail(), kept on complete() for sandbox lifetime.
    SharedRef,
    /// WarmFork Exclusive or Continuation: exclusive ownership; template consumed atomically;
    /// released back to pool on fail(), deleted on success after sandbox stops.
    Exclusive,
}

/// Phase tracker and structured error formatter for a restore operation.
/// Intentional deviation from the design: does not own the lease, PreparedMemory,
/// or PreparedBlock. Those are managed by start_with_template() and cleaned up
/// through TemplateLease::fail() and vm.stop(). Consolidate into the unified
/// RestoreTransaction ownership model when rollback complexity grows.
pub(crate) struct RestoreTransaction {
    sandbox_id: String,
    work_dir: PathBuf,
    phase: RestorePhase,
}

pub(crate) enum RestorePhase {
    RestoreVm,
    InitClient,
    /// Environment only: hotplug tap/macvtap and run guest network setup.
    /// WarmFork only: hotplug independent network namespace for this fork instance.
    HotplugNetwork,
    /// WarmFork only: inject task parameters via guest agent task-injection API.
    InjectTask,
    /// Environment only.
    PushSandboxFiles,
    /// Environment only.
    SetupSandbox,
    /// Continuation only: instruct external network identity controller to route or attach
    /// the original virtual IP / Pod IP to this node before committing the restore.
    TransferNetworkIdentity,
    Commit,
}

/// Memory backend prepared for a single restore operation.
/// Replaces the MemoryBackend trait for Phase 1–2. The trait abstraction
/// (prepare/prefetch/release) is reserved for the Working Set Restore phase.
pub(crate) struct PreparedMemoryBackend {
    pub(crate) source_url: String,
    pub(crate) mode: MemoryRestoreMode,
    pinned_paths: Vec<PathBuf>,
}

/// Block artifacts prepared for a single restore operation.
pub(crate) struct RestoredBlockArtifacts {
    pub(crate) disk_remaps: Vec<(String, String)>,
    pub(crate) restored_paths: Vec<PathBuf>,
    pub(crate) action: RestoreBlockAction,
}

pub(crate) enum RestoreBlockAction {
    None,
    /// Environment / WarmFork: per-instance private CoW copy via reflink(2).
    ReflinkCopied,
    /// Fallback when reflink is unavailable (e.g. cross-device or unsupported fs).
    PlainCopied,
    /// Continuation: exclusive ownership transfer via rename(2) into sandbox directory.
    /// Template directory is invalidated after this operation.
    MovedExclusive,
}
```

The `KuasarSandboxer::start()` path selects between cold boot and template restore using inline conditional logic. The target enum is:

```rust
pub(crate) enum SandboxStartMode {
    FreshBoot,
    RestoreFromTemplate {
        snapshot_type: SnapshotType,
        key: TemplateKey,
        memory_mode: MemoryRestoreMode,
    },
}
```

This enum is reserved for a future refactor once the per-type restore paths stabilise.

## Test Plan

### Unit Tests

- restore transaction phases and rollback.
- Shared leases do not remove templates from the available index, and GC respects active leases.
- Exclusive consume transfers ownership and leave no dangling symlink.
- MemoryRestoreMode pinning policy.
- metadata layer serialization and version compatibility.
- Admin API key/path/status validation.

### Integration Tests

- hypervisor fresh boot → snapshot → restore → guest agent ready.
- `EnvironmentSnapshot` restore: hotplug network, push sandbox files, `setup_sandbox()`.
- networked sandboxes fall back to fresh boot if hotplug is unavailable; verify network works end-to-end after restore.
- copy/filebackend/ondemand modes cannot be broken by GC while the VM is running.
- failures at each restore phase clean up the hypervisor process, memory backend, block backend, and lease state.

### E2E Tests

- CRI sandbox creation hits an `EnvironmentSnapshot` template.
- template miss and restore failure follow fallback policy.
- sandboxer crash/restart recovers template leases and pins for running sandboxes.
- force-gc does not delete snapshot files referenced by running sandboxes.
- Phase 3 `WarmForkSnapshot` tests validate task injection protocol, independent network namespace per instance, and template lease lifecycle.
- Phase 4 `ContinuationSnapshot` tests validate singleton enforcement, atomic consume, and network identity transfer on the supported local/shared-filesystem path.

## Production Readiness

### Resource Management

- `pool-status` exposes template count, active leases, pinned bytes, and GC blocked reasons.
- GC follows SnapshotGraph dependencies. GC guards on `in_flight_restores` and `in_use_count`.
- low disk space cleanup targets old templates without leases first.

### Crash Recovery

- lease and pin state is persisted.
- startup rebuilds SnapshotGraph. Pool is rehydrated from `pooled_template.json`; consumed markers prevent re-insertion of already-consumed templates.
- consumed Exclusive templates are not reinserted; Shared active leases are recovered from running sandboxes.

### Pool Health

- hit rate, restore latency, fallback count, rollback count, and GC blocked count are observable.
- low depth triggers refill.
- high rollback rate or low hit rate should alert.

## Future Enhancements

1. **SnapshotGraph**: replace the flat `HashMap<TemplateKey, VecDeque<PooledTemplate>>` pool with the full `SnapshotGraph` (nodes + edges) model for GC ordering and restore sequencing. Required before working-set restore, memory/block deltas, or remote snapshot support.
2. **Working set restore**: hot/warm/cold page classification and prefetch for `EnvironmentSnapshot` and `WarmForkSnapshot`.
3. **`MemoryBackend` trait**: introduce the `prepare/prefetch/release` trait abstraction for local mmap, remote snapshot, and tiered memory sources.
4. **`EnvironmentSnapshot` key: guest task version**: add the guest task binary version to the auto-computed key once the sandboxer can reliably read a build-time constant from the guest image. Until then, environment template creation should fail closed when the version cannot be determined rather than silently accepting a weaker key.
5. **`template-from-bundle`**: create a `WarmForkSnapshot` from an already-prepared OCI bundle/rootfs/storage metadata with process in ready-waiting state, once containerd/snapshotter integration is available.
6. **SnapshotGraph deltas**: base snapshot, memory delta, block delta. Enables incremental `WarmForkSnapshot` re-warming without full re-snapshot.
7. **Snapshot compression, encryption, and integrity validation**.
8. **Remote snapshot and lazy block/memory fetch**.
9. **Full execution image**: memory + block + metadata + restore policy bundled as a single distributable artifact.
10. **Continuation store**: per-workload storage for `ContinuationSnapshot` (separate from the shared template pool) with workload identity index, retention policy, and recovery after sandboxer restart.
11. **Per-container restore policy**: allow different restore semantics for the main container and sidecars within the same pod (e.g. main container `WarmFork`, service-mesh sidecar fresh-start). Currently a pod has a single `SnapshotType` and all processes in the VM are governed by the same type. This is too coarse for pods where sidecars cannot implement the ready-waiting protocol. Required to safely support `WarmForkSnapshot` with standard service-mesh sidecars.
12. **`StorageContinuationBackend` interface**: define an explicit interface for cross-node block layer ownership transfer backing the `MovedExclusive` action. Must specify fencing protocol (lease, STONITH, or storage-native exclusive lock), detach/attach sequencing, and consistency guarantee. Local `rename(2)` is only a valid implementation when source and target share a filesystem; cross-node cases require one of: distributed block storage re-attach, async copy + cutover, or storage-native atomic lease transfer.
13. **`ContinuationSnapshot` distributed fencing**: singleton enforcement for cross-node restores. The `consumed` marker file is a local-only mechanism. Cross-node concurrent restore requires a distributed ownership lease: only one active owner per `(pod_uid, generation)`, the old node must be confirmed stopped or forcibly fenced before the new node commits, and sandboxer crash recovery must distinguish `consumed` / `restore-in-progress` / `committed` states to prevent split-brain.
14. **Complete template key validation**: bind PodSandboxConfig hash, network topology hash, image/block artifact content digest (not path), seccomp/AppArmor profile, CPU feature set, NUMA/memory topology, hypervisor version, sidecar set and startup order to the template key. Prevents silent configuration mismatch when different pod configurations share the same key.
15. **Environment refill registry**: add an admin/config interface for declaring the set of expected EnvironmentSnapshot profiles to the refill loop. The current refill loop is single-key and derives its key from the active VM factory config; heterogeneous environment fleets need an explicit registry or separate sandboxer instances.
16. **Structural migration (`PooledTemplate` / `SandboxStartMode`)**: migrate the Phase 1–2 flat pool model to `RuntimeSnapshot` + `SnapshotGraph` once the per-type restore paths stabilise. Impacted areas include persisted template metadata, restore dispatch, GC ordering, admin API payloads, and crash-recovery rehydration.

## Drawbacks

1. The design is complex and requires strict lifecycle and transaction handling.
2. Initial performance gains may be smaller than a full lazy-restore system, but correctness is more controllable.
3. `WarmForkSnapshot` requires the application to implement the ready-waiting protocol. This is a contract between the workload and the runtime that existing applications do not natively satisfy and must be explicitly adopted.
4. The hypervisor API schema depends on the pinned version.

## Alternatives

### Alternative 1: No Snapshot/Restore

Cold-boot a VM for every sandbox.

**Rejected**: Latency is too high for dense short-lived workloads.

### Alternative 2: Resident VM Pool

Keep idle VMs running and assign them on demand.

**Rejected**: Idle VMs continuously consume memory and are less resource-efficient than snapshots.

### Alternative 3: WarmForkSnapshot First (Skip EnvironmentSnapshot)

Skip `EnvironmentSnapshot` and directly implement process warm-fork restore.

**Rejected**: WarmForkSnapshot requires solving task injection protocol, guest agent extensions, ready-waiting quiescence contract, and sidecar coordination simultaneously. `EnvironmentSnapshot` establishes the restore foundation (transaction model, lease management, network hotplug) with significantly lower risk. Phase 3 builds on this foundation.

### Alternative 4: Restore Live Network Sessions

Restore TCP/vsock/conntrack live state.

**Rejected**: Too complex and uncertain for the initial scope. The first version restores network configuration only.
