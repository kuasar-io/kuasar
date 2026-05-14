# Virtio-Blk Storage

<!-- toc -->
- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [Phased Scope](#phased-scope)
  - [Design Overview](#design-overview)
  - [Risks and Mitigations](#risks-and-mitigations)
- [Design Details](#design-details)
  - [Architecture Boundaries](#architecture-boundaries)
  - [BlockProvider Abstraction](#blockprovider-abstraction)
  - [Configuration and Guest Parameters](#configuration-and-guest-parameters)
  - [VM Configuration Changes](#vm-configuration-changes)
  - [Sandbox Configuration File Delivery](#sandbox-configuration-file-delivery)
  - [Block Image Model](#block-image-model)
  - [Container Layer Handling](#container-layer-handling)
  - [Guest-file Protocol](#guest-file-protocol)
  - [Lifecycle Ownership Model](#lifecycle-ownership-model)
  - [Device Transaction Model](#device-transaction-model)
  - [Cleanup and Resource Management](#cleanup-and-resource-management)
  - [Security Considerations](#security-considerations)
  - [Size Estimation and File Type Policy](#size-estimation-and-file-type-policy)
- [Implementation Details](#implementation-details)
  - [Key Data Structures](#key-data-structures)
  - [Guest Protocol](#guest-protocol)
- [Test Plan](#test-plan)
- [Future Enhancements](#future-enhancements)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
<!-- /toc -->

## Summary

This document describes virtio-blk storage support for microVM runtimes in Kuasar. The backend provides a block-device path in addition to the default virtiofs path: the runtime prepares snapshot-safe container layers or directory contents as block images, then hot-attaches them to the guest through the hypervisor's block device hotplug API.

The design is intentionally phased:

- **Phase 1** prioritizes correctness and introduces an opt-in `virtio-blk` storage backend. Overlay rootfs and explicitly allowed static bind directories are converted to sandbox-private ext4 compatibility images. Small files and small directories are injected through TTRPC.
- **Phase 2** moves image preparation behind a generic `BlockProvider` boundary and introduces `DeviceGraph` metadata. The basic boundary exists today; snapshot/restore integration, EROFS, dm-verity, reflink, and remote lazy-block support remain future work.
- **Phase 3** supports immutable block images produced by the snapshotter. The runtime owns device lifecycle and guest mounting, but does not directly interpret OCI layer semantics.

`virtiofs` remains the default backend. `virtio-blk` must be enabled explicitly.

## Motivation

### Goals

1. Provide a storage backend that does not require virtiofsd.
2. Reduce helper-process and shared-memory dependencies for minimal VM deployments.
3. Support snapshot-safe overlay rootfs and explicitly static bind mount use cases.
4. Define ownership boundaries between the runtime, snapshotter, sandbox, and guest mount state.
5. Align storage with snapshot/restore by making block images copyable, reflinkable, pinnable, and verifiable.
6. Preserve backward compatibility for the default virtiofs deployment.

### Non-Goals

1. Hypervisors that do not support virtio-blk device hotplug are out of scope for the initial implementation.
2. Large-scale performance optimization in the first phase.
3. Host-side writable-layer CoW, delta merge, or writeback.
4. Direct OCI layer parsing, image pulling, or snapshotter metadata ownership inside the virtio-blk backend.
5. Transparent support for HostPath, sockets, log directories, or device files that require live host/guest coherence.
6. Real-time file synchronization between host and guest.

## Proposal

### User Stories

#### Story 1: Minimal VM Deployment without virtiofsd

Users can run microVM sandboxes without installing or running virtiofsd. The runtime supplies rootfs and static inputs through virtio-blk devices and TTRPC file delivery.

#### Story 2: Snapshot-friendly Storage Backend

Users can restore VM snapshots that reference block images. This is easier to copy, reflink, pin, and validate than a vhost-user socket bound to a live virtiofsd process.

#### Story 3: Immutable Execution Environment

Long term, the snapshotter or image service can produce immutable EROFS/dm-verity block images. The runtime mounts those images through virtio-blk, and the guest performs integrity validation.

### Phased Scope

Phase 1 behavior:

- Overlay rootfs is converted to a sandbox-private ext4 image. It is not a shared cross-sandbox cache.
- Writable state is not implemented as host-side block CoW. Writes happen in the sandbox-private image or in a guest-local scratch/overlay upper.
- Bind mounts are converted only when they are explicitly declared as static snapshots.
- Sockets, device nodes, live log paths, and live HostPath mounts are rejected by default. Users can keep using virtiofs for those semantics.

Long-term architecture:

```text
OCI Image
  -> snapshotter / image service
  -> Chunked Immutable Image
  -> Block Mapping Layer
  -> BlockProvider
  -> virtio-blk / future block transport
  -> guest EROFS or dm-verity mount
```

The runtime consumes block artifacts; it does not own OCI metadata.

### Design Overview

```text
Host
  Kuasar sandboxer
    +-- Sandbox config files
    |   -> TTRPC push to /run/kuasar/state
    +-- BlockProvider
    |   +-- LocalBlockProvider
    |   +-- FutureErofsProvider
    |   +-- FutureSnapshotBlockProvider
    +-- DeviceGraph
    |   -> tracks storage identity, device, guest mount, cleanup owner
    +-- hypervisor block hotplug API

Guest
  /run/kuasar/state
  virtio-blk device
  guest storage handler
    -> resolve PCI BDF
    -> mount ext4/erofs/dm-verity target
    -> OCI spec uses guest mount path
```

### Risks and Mitigations

#### Risk 1: Lifecycle and Metadata Complexity

Block images, hotplugged devices, guest mounts, snapshot references, and cleanup paths cross component boundaries.

**Mitigations**:

- Define an explicit lifecycle ownership model.
- Persist `cleanup_path`, `owned_by_runtime`, `readonly`, and lease state for every block artifact.
- Model dependencies with `DeviceGraph` instead of a raw `Vec<BlockDevice>`.

#### Risk 2: Bind Mount Semantics Change

virtio-blk provides a static snapshot, not live virtiofs-style sharing.

**Mitigations**:

- `container_storage_backend="virtio-blk"` is explicit opt-in.
- Live HostPath, sockets, devices, and log directories are rejected by default.
- Snapshot-safe bind mounts require config or annotation opt-in.

#### Risk 3: Startup Latency and Disk Usage

Phase 1 must create ext4 images and copy content.

**Mitigations**:

- Use TTRPC injection for small files and small directories.
- Use sparse ext4 images with explicit cleanup ownership.
- Move toward immutable EROFS, reflink, chunk dedupe, and lazy block fetch in later phases.

#### Risk 4: Partial Attach or Restore

Failures can leave metadata written without a mounted device, or a device attached but not recognized by the guest.

**Mitigations**:

- Use a prepare -> attach -> commit -> rollback transaction for host-side block artifacts and hotplug.
- Add guest-side uevent, mount, fstype, and readonly verification as a future confirmation step.
- Do not persist sandbox storage metadata before commit.

## Design Details

### Architecture Boundaries

```text
OCI / image semantics
  owned by snapshotter or image service

Block artifact
  produced by BlockProvider

Device attachment
  owned by Kuasar runtime

Guest mount state
  owned by sandbox + guest storage handler

Snapshot reference
  owned by snapshot/restore subsystem
```

This prevents the virtio-blk backend from depending on OCI layer layout, image digests, diff IDs, or snapshotter internals.

### BlockProvider Abstraction

`BlockProvider` turns a mountable input into a block artifact. The virtio-blk backend only consumes the returned `BlockArtifact`.

```rust
#[async_trait]
trait BlockProvider {
    async fn prepare(&self, req: BlockPrepareRequest) -> Result<BlockArtifact>;
    async fn release(&self, artifact: &BlockArtifact) -> Result<()>;
}

struct BlockArtifact {
    path: String,
    fstype: String,
    readonly: bool,
    owned_by_runtime: bool,
    cleanup_path: Option<String>,
    source_identity: Option<String>,
}
```

Initial implementation:

- `LocalBlockProvider`: creates sandbox-private ext4 images from overlays or static directories. It also contains reserved XFS/reflink helpers, but the current storage paths request `BlockFs::Ext4`.

Future implementations (out of scope for this feature):

- `ErofsProvider`: consumes immutable compressed EROFS images.
- `SnapshotBlockProvider`: restores block images from snapshot metadata. This provider is explicitly deferred to the snapshot/restore feature track. It requires the snapshot metadata schema, `SnapshotGraph`, and `TemplateLease` infrastructure defined in the snapshot/restore design to be available first. Do not implement `SnapshotBlockProvider` within the virtio-blk feature.
- `RemoteLazyProvider`: supports chunk mapping and remote lazy fetch.

### Configuration and Guest Parameters

Example configuration:

```toml
[hypervisor]
container_storage_backend = "virtiofs" # virtiofs | virtio-blk

[hypervisor.virtio_blk]
allow_bind_snapshot = false
default_fstype = "ext4" # reserved; current attach paths explicitly request ext4
block_image_size_overhead_percent = 20
small_dir_max_files = 50
small_dir_max_bytes = 10485760
overlay_image_fallback_size_mb = 64
bind_image_fallback_size_mb = 8

[hypervisor.virtiofsd]
path = "/usr/local/bin/virtiofsd"
log_level = "info"
cache = "never"
thread_pool_size = 4
```

Guest parameters must remain compatible with the existing `task.sharefs_type` model:

- virtiofs mode continues to pass `task.sharefs_type=virtiofs`.
- virtio-blk mode passes `task.sharefs_type=none task.container_storage_backend=virtio-blk`.
- guest `TaskConfig` must recognize `none`, skip 9p/virtiofs mounting, and create `/run/kuasar/state`.

Adding only `task.container_storage_backend` is not sufficient if `task.sharefs_type` still uses a sharefs value; virtio-blk mode must set both parameters consistently.

### VM Configuration Changes

In virtio-blk mode:

- shared memory is not required unless another device explicitly needs it.
- virtiofsd is not started.
- no virtio-fs device is added to the VM config.
- block hotplug uses the hypervisor's block device API and records the returned device identifier (e.g. PCI BDF).

virtiofs mode keeps the existing behavior.

For the Cloud Hypervisor initial implementation: `memory.shared=false`, `CloudHypervisorVM::start()` records only actually spawned affiliated pids, and hotplug uses `vm.add-disk` with the returned PCI BDF.

### Sandbox Configuration File Delivery

Because there is no shared filesystem in virtio-blk mode, sandbox files must be pushed before `setup_sandbox()`:

- `hostname` -> `/run/kuasar/state/hostname`
- `resolv.conf` -> `/run/kuasar/state/resolv.conf`
- `hosts` -> `/run/kuasar/state/hosts`

Flow:

1. The guest creates `/run/kuasar/state` during initialization.
2. The host reads the files from the sandbox shared directory.
3. The host pushes content through `ExecVMProcess` or a future structured file-write RPC.
4. Writes target fixed guest paths only; CRI-provided paths are never passed through directly.

### Block Image Model

Phase 1 uses ext4 compatibility images:

- overlay rootfs: sandbox-private ext4 image.
- static large directory: sandbox-private ext4 image.
- writeability follows mount semantics, but the image is never shared across sandboxes.
- `default_fstype` and XFS/reflink helpers are reserved for future policy; current overlay and large-bind paths explicitly pass `BlockFs::Ext4`.

Long-term immutable model:

- the snapshotter produces read-only EROFS images.
- the guest can stack dm-verity for integrity verification.
- the runtime consumes the image through `BlockProvider` and does not parse OCI metadata.
- writable upper initially remains guest-local overlay/scratch; host-side block CoW upper is out of scope.

### Container Layer Handling

#### Overlay rootfs

Phase 1 flow:

1. Mount the overlay to a temporary host directory.
2. `LocalBlockProvider` creates an ext4 image from the content.
3. Unmount temporary overlay and loop mounts.
4. Hot-add the image through the hypervisor block hotplug API.
5. The guest resolves PCI BDF and mounts it at `/run/kuasar/storage/containers/{storage_id}`.
6. Sandbox storage metadata is persisted only after commit.

#### Bind mounts

Bind mounts are classified first:

- single regular file: TTRPC guest-file injection.
- small regular directory: TTRPC guest-file injection.
- large directory explicitly declared as static snapshot: block image.
- socket, device node, fifo, live writeback path, or live read path: reject or require virtiofs.

Directory bind mounts are static snapshots, not transparent live shares.

### Guest-file Protocol

`guest-file` means the runtime has already placed the file or directory inside the guest, and the OCI spec can bind mount that guest path directly.

Rules:

- `guest-file` is not included in `io.kuasar.storages` and is not sent to the guest storage handler.
- `need_guest_handle=false`.
- `StorageHandler` may rewrite the OCI mount source only after injection has completed.
- rootfs never uses `guest-file`; rootfs must use a block device or fail.
- guest-file injection rejects symlinks.
- block-image large directory delivery preserves symlinks through the image copy path.
- special files such as sockets, FIFOs, and device nodes are rejected.

### Lifecycle Ownership Model

| Object | Owner | Release point |
|---|---|---|
| snapshotter immutable image | snapshotter/image service | image GC |
| phase 1 temporary ext4 image | Kuasar runtime | storage refcount reaches 0 and guest unmounts |
| hypervisor block device | Kuasar runtime | container deletion or restore rollback |
| guest mount | guest storage handler | unmount during container deletion |
| snapshot reference | snapshot/restore subsystem | sandbox lease release |

Runtime-created images must carry `cleanup_path`. The runtime must not infer cleanup ownership from `fstype == "ext4"` because external ext4 block devices can be hot-attached too.

### Device Transaction Model

Current block attach uses a host-side transaction. Snapshot-restore block recovery should follow the same lifecycle, but the unified snapshot provider is still future work:

```text
prepare
  create or locate BlockArtifact
  validate path, size, readonly, integrity

attach
  hypervisor block hotplug API
  record device_id and device identifier (e.g. BDF)

commit
  persist sandbox storage metadata
  add DeviceGraph reference

rollback
  call hypervisor block remove API if attached
  release BlockArtifact if owned_by_runtime
```

The proposal still requires a future guest confirmation step that waits for the uevent, resolves BDF -> `/dev/vdX`, mounts the device, and verifies fstype/readonly before commit.

### Cleanup and Resource Management

Cleanup is based on `DeviceGraph` and ownership, not filesystem type:

```rust
struct DeviceGraphNode {
    storage_id: String,
    device_id: Option<String>,
    bdf: Option<String>,
    artifact: BlockArtifact,
    guest_mount: Option<String>,
    ref_count: u32,
}
```

Container deletion:

1. unmount in the guest.
2. call the hypervisor block remove API on the host.
3. call `BlockProvider::release()` for runtime-owned temporary images.
4. update sandbox dump.

### Security Considerations

#### Path validation

Character allowlists are not sufficient. The design uses semantic validation:

- `container_id` and `storage_id` allow only `[A-Za-z0-9_.:-]`; `/` and `..` are rejected.
- guest paths are built from path components and must remain under `/run/kuasar/state` or `/run/kuasar/storage/containers`.
- host input paths are canonicalized and validated against the mount policy.

#### Shell use

Short term, `ExecVMProcess` may be used, but commands must be fixed templates and dynamic content must be passed through stdin. Long term, structured file-write and mkdir RPCs should replace shell command composition.

#### Integrity

Long-term immutable block images should use dm-verity inside the guest. The host supplies raw immutable image and verity metadata, which keeps the trust boundary clear for confidential containers and remote attestation.

### Size Estimation and File Type Policy

ext4 sizing must consider both data blocks and inode pressure:

- count total bytes, files, directories, xattrs, and hardlinks.
- pass `mkfs.ext4 -N <inode_count>` for many small files.
- keep a minimum size and configurable overhead.
- explicitly support or reject sparse files, hardlinks, xattrs, device nodes, fifos/sockets, and symlinks.

No file type may be silently dropped.

## Implementation Details

### Key Data Structures

```rust
pub enum ContainerStorageBackend {
    Virtiofs,
    VirtioBlk,
}

pub struct BlockPrepareRequest {
    pub src_dir: String,
    pub img_path: String,
    pub fstype: BlockFs,
    pub fallback_size_mb: u64,
    pub overhead_percent: u32,
    pub source_identity: Option<String>,
    pub readonly: bool,
}

pub struct BlockArtifact {
    pub path: String,
    pub fstype: String,
    pub readonly: bool,
    pub owned_by_runtime: bool,
    pub cleanup_path: Option<String>,
    pub source_identity: Option<String>,
}

pub struct Storage {
    pub host_source: String,
    pub r#type: String,
    pub id: String,
    pub device_id: Option<String>,
    pub ref_container: HashMap<String, u32>,
    pub need_guest_handle: bool,
    pub source: String,      // BDF for block, guest path for guest-file
    pub driver: String,      // blk/scsi/guest-file host-side marker
    pub driver_options: Vec<String>,
    pub fstype: String,
    pub options: Vec<String>,
    pub mount_point: String,
    pub lower_dirs: Option<String>,
    pub cleanup_path: Option<String>,
    pub owned_by_runtime: bool,
    pub source_identity: Option<String>,
}
```

`cleanup_path` is only for runtime-owned resources and must remain backward-compatible when older sandbox state is loaded.

### Guest Protocol

The guest storage handler supports:

- `driver=blk`: wait for the block device by PCI BDF and mount `source`.
- `driver=scsi`: keep the existing SCSI behavior.
- `driver=ephemeral`: keep the existing tmpfs behavior.

`guest-file` is not sent to the guest storage handler; the OCI mount source points directly at the injected guest path.

## Test Plan

### Unit Tests

- `ContainerStorageBackend` config parsing and default behavior.
- guest parameters: virtiofs keeps `task.sharefs_type=virtiofs`; virtio-blk uses `task.sharefs_type=none task.container_storage_backend=virtio-blk`.
- path component validation rejects `/`, `..`, and shell-special paths.
- `guest-file` is omitted from storage annotations.
- `BlockProvider::release()` deletes only `owned_by_runtime=true` artifacts.
- inode and size estimation handles many small files.

### Integration Tests

- virtio-blk mode starts a sandbox without virtiofsd and without a virtio-fs device.
- overlay rootfs creates an ext4 image, hotplugs it, mounts it in the guest, and runs a container.
- bind file, small directory, and large directory cases verify content, permissions, readonly options, and cleanup.
- unsupported sockets/devices/live HostPath mounts fail clearly.
- failures at each attach phase roll back device, mount, and temporary image state.

### E2E Tests

- run a basic pod with the virtio-blk storage backend.
- after container deletion, verify guest unmount, hypervisor block device removal, and temporary image cleanup.
- with snapshot/restore, verify block image leases are not garbage-collected while active.

## Future Enhancements

1. **Immutable EROFS images** produced by the snapshotter.
2. **dm-verity** validation inside the guest.
3. **Block Mapping Layer** for logical-block to chunk mapping.
4. **Remote lazy block fetch** for remote images.
5. **Multi-queue virtio-blk** through `num_queues`.
6. **DeviceGraph** expansion for base images, snapshot deltas, scratch disks, and cache disks.
7. **Other block transports** such as virtio-scsi, virtio-pmem, vhost-user-blk, and NVMe passthrough.

## Drawbacks

1. Phase 1 ext4 image creation increases startup latency and disk usage.
2. Static bind snapshots do not provide virtiofs live-sharing semantics.
3. Lifecycle and rollback handling are more complex than virtiofs.
4. The initial phase does not yet provide the long-term EROFS/dm-verity/lazy-fetch benefits.

## Alternatives

### Alternative 1: virtiofs Only

Continue using virtiofs as the only backend.

**Rejected**: It does not meet the goals of no virtiofsd, snapshot-friendly storage, and block-native execution.

### Alternative 2: virtio-9p Backend

Use virtio-9p instead of virtio-blk.

**Rejected**: Its performance and semantics are not suitable for snapshot-native microVMs.

### Alternative 3: Direct Block Device Passthrough

Pass host block devices directly to the VM.

**Rejected**: overlay rootfs and snapshotter artifacts are usually not physical block devices, and ownership/GC risks are higher.

### Alternative 4: Immediate Host-side Block CoW Writable Layer

Implement host-side block CoW and delta merge for writable upper layers.

**Rejected**: This turns the first phase into a storage engine project and greatly increases consistency, crash recovery, and GC complexity.
