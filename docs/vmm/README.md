# Architecture
Kuasar-sandboxer is a sandboxer plugin of containerd. a sandboxer is a component of containerd for container sandbox lifecycle management. A sandbox should provide a set of task API to containerd for container lifecycle management. the `containerd-task-kuasar` the PID 1 process running in the vm launched by vmm-sandboxer, it provides task API with the vsock connection.
![](images/arch.png)

# Installation Guide

## Prerequisites
kuasar should be running on bare metal of x86_64 arch, HostOS should be linux with of 4.8 or higher, with hypervisor installed(qemu support currently, and cloud-hypervisor will be supported soon), Containerd with CRI plugin is also required. rust toolchains is required for compiling the source.

## Building from source

`x86_64-unknown-linux-musl` should be installed to make easy-to-deploy static linked or minimally dynamic linked programs in the building of `vmm-task`.

Build it with root user:

```sh
rustup target add x86_64-unknown-linux-musl
make bin/vmm-sandboxer
make bin/vmm-task
```

Additionally, kuasar dir should be created: `mkdir -p /var/lib/kuasar`.

## Building guest kernel
Guest kernel should also be linux of 4.8 or higher, with virtio-vsock enabled, make sure [this patch](https://lore.kernel.org/all/20191122070009.5CE442068E@mail.kernel.org/T/) is merged.
```
CONFIG_VSOCKETS=y
CONFIG_VSOCKETS_DIAG=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS_COMMON=y
```

Run `make bin/vmlinux.bin` and `cp bin/vmlinux.bin /var/lib/kuasar/vmlinux.bin`

## Building guest os image

The guest os can be either a busybox or any linux distributions, but make the sure that the `vmm-task` be the init process. and make sure runc is installed.

Run `make bin/kuasar.img` and `cp bin/kuasar.img /var/lib/kuasar/kuasar.img`

## Config
Config cloud-hypervisor vmm-sandboxer, should copy the config file in vmm directory by `cp vmm/sandbox/config_clh.toml /var/lib/kuasar/config_clh.toml`.

The default config looks like this:
```toml
[sandbox]
[hypervisor]
  path = "/usr/local/bin/cloud-hypervisor"
  vcpus = 1
  memory_in_mb = 2048
  kernel_path = "/var/lib/kuasar/vmlinux.bin"
  image_path = "/var/lib/kuasar/kuasar.img"
  initrd_path = ""
  kernel_params = ""
  hugepages = false
  entropy_source = "/dev/urandom"
  debug = true
[hypervisor.virtiofsd]
  path = "/usr/local/bin/virtiofsd"
  log_level = "info"
  cache = "never"
  thread_pool_size = 4
```

# Note

Please note that this guide only teach you how to build kuasar from source code, if you want to run the kuasar, cloud hypervisor and virtiofsd are also needed!
