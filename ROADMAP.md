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

+ Support parts of mainstream sandbox, i.e. Cloud Hypervisor, QEMU, StratoVirt, WasmEdge, Quark
+ Support connection of containerd and iSulad via Sandbox API

## 2023 H2

+ Support runC container
+ Support Wasmtime
+ Start the process of donating projects to CNCF

## 2024

+ Support more sandboxes, i.e. gVisor, Firecracker
+ Support running in Arm64.
+ Develop a CLI tool for operation and maintenance.

## 2025

+ Enhance on image distribution.
+ Support eBPF observation.
+ TBD
