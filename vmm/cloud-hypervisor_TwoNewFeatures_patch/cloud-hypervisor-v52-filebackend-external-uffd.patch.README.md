# Cloud Hypervisor v52.0 File-backend MMAP and External UFFD Restore Patch

# Note: the following steps are tested under 22.04.1-Ubuntu/kernel: vmlinuz-6.8.0-111-generic 

## 1. Base Version

This patch is based on Cloud Hypervisor v52.0.

Base commit:

1314ac883c641f1045bbb06dec0de045a3894baa
build: Release v52.0

## 2. Patch Contents

This patch adds two snapshot restore features:

  1) File-backend MMAP restore
  2) External UFFD restore

It also includes an external UFFD handler example:

  cloud-hypervisor/examples/external_uffd_handler.rs

## 3. Git clone Cloud Hypervisor and checkout v52.0,then apply patch

### 3.1 Git clone Cloud Hypervisor and checkout v52.0
  mkdir -p ~/cloud-hypervisor
  cd ~/cloud-hypervisor

  git clone https://github.com/cloud-hypervisor/cloud-hypervisor.git cloud-hypervisor-v52
  cd cloud-hypervisor-v52

Start from the official v52.0 release tag.

  git checkout v52.0

Confirmation:

  git log --oneline -n 3
  git status

You should see something like:
  1314ac883 (HEAD, tag: v52.0) build: Release v52.0
  HEAD detached at v52.0

If you want to develop on v52.0, it is recommended to create a branch:
  git checkout -b apply-filebackend-external-uffd-v52

### 3.2 Apply Patch

  git checkout -b apply-filebackend-external-uffd-v52
  git log --oneline -n 3
  git status

You still see something like:
  1314ac883 (HEAD, tag: v52.0) build: Release v52.0
  HEAD detached at v52.0

Optional but recommended: verify the patch can be applied cleanly.

  git apply --check /path/to/kuasar/vmm/cloud-hypervisor_TwoNewFeatures_patch/cloud-hypervisor-v52-filebackend-external-uffd.patch

Apply the plain diff patch

  git apply /path/to/kuasar/vmm/cloud-hypervisor_TwoNewFeatures_patch/cloud-hypervisor-v52-filebackend-external-uffd.patch
  git status

## 4. Build and verify

  cargo build --release
  cargo build --release --example external_uffd_handler

If building in an offline/cached environment:

  cargo build --release --offline
  cargo build --release --example external_uffd_handler --offline

Review the applied changes:

  git status
  git diff --stat

Install:
  ls -alg target/release/cloud-hypervisor target/release/examples/external_uffd_handler

  sudo cp -pr target/release/cloud-hypervisor /usr/local/bin/cloud-hypervisor-v52
  sudo cp -pr target/release/examples/external_uffd_handler /usr/local/bin/external_uffd_handler-v52
  sudo chmod 755 /usr/local/bin/cloud-hypervisor-v52 /usr/local/bin/external_uffd_handler-v52
  sudo ls -alg target/release/cloud-hypervisor /usr/local/bin/cloud-hypervisor-v52



*****************************************************
* Note: All commands below require root privileges. *
*****************************************************
## 5. Make Rootfs

### 5.1 prerequisite work
Ensure docker image nginx-slim:0.21 is in your local docker image repository

  # docker image ls nginx-slim:0.21

Ensure to copy kernel from directory /boot to /tmp/sandbox/boot/

  # mkdir -p /tmp/sandbox/boot/
  # cp -pr /boot/vmlinuz /tmp/sandbox/boot/vmlinuz
  # file /tmp/sandbox/boot/vmlinuz 

Set environment variables

  # export CH=/usr/local/bin/cloud-hypervisor-v52
  # export KERNEL=/tmp/sandbox/boot/vmlinuz
  # export W=/tmp/unified-bench-kuasar

  # mkdir -p $W
  # cd $W
  # pwd

  # cp -pr /path/to/kuasar/vmm/cloud-hypervisor_TwoNewFeatures_patch/SnapShotScripts/create_rootfs_slim.v52.sh .
  # cp -pr /path/to/kuasar/vmm/cloud-hypervisor_TwoNewFeatures_patch/SnapShotScripts/create_snapshot-externalUFFD.v52.sh .
  # ls -alg $W

### 5.2 Export rootfs from a Docker image and inject the /init script into rootfs
  Note: ensure you are in directory /tmp/unified-bench-kuasar

  # pwd
  # bash create_rootfs_slim.v52.sh

## 6. Create Snapshot

  # bash create_snapshot-externalUFFD.v52.sh

## 7. Restore Tests
### 7.1 Copy Restore
  # /usr/local/bin/cloud-hypervisor-v52   --api-socket /tmp/unified-bench-kuasar/ch-api.sock   --restore "source_url=file:///tmp/unified-bench-kuasar/snap-slim-externalUFFD",memory_restore_mode=copy   -vvv

  Note: you should see message like "cloud-hypervisor:   2.131366s: <vmm> INFO:event_monitor/src/lib.rs:113 -- Event: source = vm event = restored"

  Note: -vvv means debug level
  Note: type Ctrl+C to close test



### 7.2 OnDemand / Internal UFFD Restore
  # /usr/local/bin/cloud-hypervisor-v52   --api-socket /tmp/unified-bench-kuasar/ch-api.sock   --restore "source_url=file:///tmp/unified-bench-kuasar/snap-slim-externalUFFD",memory_restore_mode=ondemand   -vvv

  Note: you should see message like "cloud-hypervisor:   0.032005s: <vmm> INFO:event_monitor/src/lib.rs:113 -- Event: source = vm event = restored"

  Note: type Ctrl+C to close test

### 7.3 File-backend MMAP Restore
  # /usr/local/bin/cloud-hypervisor-v52   --api-socket /tmp/unified-bench-kuasar/ch-api.sock   --restore "source_url=file:///tmp/unified-bench-kuasar/snap-slim-externalUFFD",memory_restore_mode=filebackend   -vvv

  Note: you should see message like "cloud-hypervisor:   0.029752s: <vmm> INFO:event_monitor/src/lib.rs:113 -- Event: source = vm event = restored"

  Note: type Ctrl+C to close test

### 7.4 External UFFD Restore

  Start handler in terminal #1:

  # /usr/local/bin/external_uffd_handler-v52

  Run restore in terminal #2: 

  # /usr/local/bin/cloud-hypervisor-v52   --api-socket /tmp/unified-bench-kuasar/ch-api.sock   --restore "source_url=file:///tmp/unified-bench-kuasar/snap-slim-externalUFFD",memory_restore_mode=externalUFFD   -vvv

  Note: you should see message like "cloud-hypervisor:   0.030715s: <vmm> INFO:event_monitor/src/lib.rs:113 -- Event: source = vm event = restored"

  Note: type Ctrl+C on both terminals to close test

## 8. Notes
  The patch was validated on Cloud Hypervisor v52.0.
  The external UFFD handler should be started before running the external UFFD restore script.
  Default VMM restore output is kept clean; detailed UFFD logs are moved to debug-level logging.
