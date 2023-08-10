# Roadmap

This document defines a high level roadmap for Kuasar development.

## 2023 H1

### Core framework

#### Sandbox API
+ Define and develop new Sandbox API with containerd team

#### Integration of high-level container runtime
+ Containerd (of Kuasar community)
+ Containerd (of Containerd community with kuasar-shim)
+ iSulad

#### Sandbox and container management
+ Cloud Hypervisor
+ QEMU
+ StratoVirt
+ WasmEdge
+ QuarkContainer

### Test
+ Performance testing towards on startup time and memory overhead of kuasar vmm sandbox

## 2023 H2

### Core framework

#### Integration of high-level container runtime
+ Containerd (of Containerd community)

#### Sandbox and container management
+ Wasmtime
+ Runc

#### Kubernetes feature support
+ Support Kubernetes Dynamic Resource Allocation (DRA) and Node Resource Interface (NRI)
+ Support Evented PLEG

#### Enhancement features
+ Support CgroupV2

### Maintainability
+ More observabilities to the project by opentracing
+ Enhancement of sandboxer recovery

### Security
+ Complete security vulnerability scanning

### Test
+ Building e2e test workflow with more scenarios

## 2024

### Core framework

#### Sandbox and container management
+ gVisor
+ Firecracker

#### Enhancement features
+ Support CgroupV2
+ Running vm on Container OS

#### Kubernetes feature support
+ Container checkpointing
+ In-place Update of Pod Resources

### Maintainability
+ Develop CLI tool for operation and maintenance

## 2025

### Enhancement features
+ Image distribution
+ eBPF observation
