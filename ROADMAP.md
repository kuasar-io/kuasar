# Roadmap

| Sandboxer      | Sandbox          | 2023 H1 | 2023 H2 | 2024 H1 |
|----------------|------------------|---------|---------|---------|
| **MicroVM**    | Cloud Hypervisor | ✓       |         |         |
|                | QEMU             | ✓       |         |         |
|                | StratoVirt       | ✓       |         |         |
|                | Firecracker      |         |         | ✓       |
| **App Kernel** | Quark            | ✓       |         |         |
|                | gVisor           |         |         | ✓       |
| **Wasm**       | WasmEdge         | ✓       |         |         |
|                | Wasmtime         |         | ✓       |         |
| **runC**       | runC             |         | ✓       |         |


## 2023 H1

+ Release v0.1
+ Support parts of mainstream sandbox, i.e. Cloud Hypervisor, QEMU, StratoVirt, WasmEdge, Quark
+ Support connection of containerd and iSulad via Sandbox API

## 2023 H2

+ Support runC container
+ Support Wasmtime
+ Start the process of donating projects to CNCF

## 2024 H1

+ Release v1.0
+ Support more sandboxes, i.e. gVisor, Firecracker
+ Develop a CLI tool for operation and maintenance.

## 2024 H2

+ Support running in Arm64.
+ Enhance on image distribution.

## 2025

+ Release v2.0
+ Support eBPF observation.
+ TBD
