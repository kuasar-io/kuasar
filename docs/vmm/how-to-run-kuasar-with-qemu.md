
## Prerequisites

Kuasar should be running on bare metal with x86_64 or aarch64 architecture, with a Linux kernel version 4.8 or higher. QEMU must be installed and available, along with containerd with CRI plugin support. 

> Note: All of the following commands need to run with root privilege.

## Install QEMU

```bash
$ apt install qemu-system-x86
```
- If you use another Linux distribution OS, you can build the qemu from the source and install it: [Build qemu](https://github.com/qemu/qemu/blob/master/README.rst)

- After you build or install the qemu package, you can find the following important binary file in your server:
- For x86_64 architecture:
```bash
# The default QEMU path expected by Kuasar
$ ls /usr/bin/qemu-system-x86_64
/usr/bin/qemu-system-x86_64
```

- For aarch64 architecture:
```bash
# The default QEMU path expected by Kuasar
$ ls /usr/bin/qemu-system-aarch64
/usr/bin/qemu-system-aarch64
``` 

## Build and Install Kuasar VMM Sandboxer

### Build requirement

Kuasar use  `docker` or `containerd` container engine to build guest os initrd image, so you need to **make sure `docker` or `containerd` is correctly installed and can pull the image from the dockerhub registries**.

> Tips: `make vmm` build command will download the Rust and Golang packages from the internet, so you need to provide the `http_proxy` and `https_proxy` environments for the `make all` command.
>
> If a self-signed certificate is used in the `make all` build command execution environment, you may encounter SSL issues with downloading resources from https URL failed. Therefore, you need to provide a CA-signed certificate and copy it into the root directory of the Kuasar project, then rename it as "proxy.crt". In this way, our build script will use the "proxy.crt" certificate to access the https URLs of Rust and Golang installation packages.

### Build Process

```bash
# Build kuasar vmm-sandboxer with QEMU hypervisor
$ HYPERVISOR=qemu make vmm

# Install kuasar vmm-sandboxer
$ HYPERVISOR=qemu make install-vmm
``` 

This will install the required files:
- `/usr/local/bin/vmm-sandboxer` - vmm-sandboxer binary

- `/var/lib/kuasar/vmlinux.bin` - kernel binary

- `/var/lib/kuasar/kuasar.initrd` - initrd image file

- `/var/lib/kuasar/config.toml` - QEMU configuration file 

## Build and configure Containerd

### Build containerd

Since some code has not been merged into the upstream containerd community, so you need to manually compile the containerd source code in the [kuasar-io/containerd](https://github.com/kuasar-io/containerd.git).

git clone the codes of containerd fork version from kuasar repository.
```bash
$ git clone -b v0.2.0-kuasar https://github.com/kuasar-io/containerd.git
$ cd containerd
$ make bin/containerd
$ install bin/containerd /usr/bin/containerd
```

### Configure containerd

Add the following sandboxer config in the containerd config file `/etc/containerd/config.toml`

```toml
    [proxy_plugins]
      [proxy_plugins.vmm]
        type = "sandbox"
        address = "/run/vmm-sandboxer.sock"
    
    [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-vmm]
      runtime_type = "io.containerd.kuasar-vmm.v1"
      sandboxer = "vmm"
      io_type = "streaming"
```

## Configure crictl

### Install CNI plugin

Install [cni-plugin](https://github.com/containernetworking/plugins/)  which is required by [crictl-tools](https://github.com/kubernetes-sigs/cri-tools) to configure pod network.

```bash
$ wget https://github.com/containernetworking/plugins/releases/download/v1.2.0/cni-plugins-linux-arm64-v1.2.0.tgz

mkdir -p /opt/cni/bin/
mkdir -p /etc/cni/net.d

tar -zxvf cni-plugins-linux-arm64-v1.2.0.tgz -C /opt/cni/bin/
```

### Install and configure crictl

```bash
VERSION="v1.15.0" # check latest version in /releases page
wget https://github.com/kubernetes-sigs/cri-tools/releases/download/$VERSION/crictl-$VERSION-linux-arm64.tar.gz
sudo tar zxvf crictl-$VERSION-linux-arm64.tar.gz -C /usr/local/bin
rm -f crictl-$VERSION-linux-arm64.tar.gz
```

create the crictl config file in the `/etc/crictl.yaml`
```bash
cat /etc/crictl.yaml
# isulad container engine configuraton
#runtime-endpoint: unix:///var/run/isulad.sock
#image-endpoint: unix:///var/run/isulad.sock

# containerd container engine configuration
runtime-endpoint: unix:///var/run/containerd/containerd.sock
image-endpoint: unix:///var/run/containerd/containerd.sock
timeout: 10
```

## Run pod and container with crictl

### Configure config.toml file

The default config file `/var/lib/kuasar/config.toml` for QEMU vmm-sandboxer:
```toml
[sandbox]
# set kuasar log level, (default: info)
log_level = "info"

[hypervisor]
# set memory size for each sandbox in MB, (default: 2048)
memory_in_mb = 2048
# set number of vcpus for each sandbox, (default: 1)
vcpus = 1
# kernel boot parameters for guest OS
kernel_params = "task.log_level=debug task.sharefs_type=9p tsc=reliable rcupdate.rcu_expedited=1 i8042.direct=1 i8042.dumbkbd=1 i8042.nopnp=1 i8042.noaux=1 noreplace-smp= reboot=k console=hvc0 console=hvc1 iommu=off cryptomgr.notests= net.ifnames=0 pci=lastbus=0"
# set guest kernel path, (default: /var/lib/kuasar/vmlinux.bin)
kernel_path = "/var/lib/kuasar/vmlinux.bin"
# set guest initrd path, (default: "")
initrd_path = "/var/lib/kuasar/kuasar.initrd"
# enable machine-specific accelerators, (default: "")
machine_accelerators = ""
# custom firmware path, (default: "")
firmware_path = ""
# CPU feature flags, (default: "")
cpu_features = ""
# CPU model, (default: "host")
cpu_model = "host"
# set qemu_path, (default: /usr/bin/qemu-system-x86_64)
qemu_path = "/usr/bin/qemu-system-x86_64"
# set the type of the analog chip, "virt" for ARM architecture and "pc" for x86 architecture, (default: pc)
machine_type = "pc"
# number of default network bridges, (default: 1)
default_bridges = 1
# the maximum number of vCPUs allocated for the VM, (default: 0)
default_max_vcpus = 0
# entropy source for guest RNG, (default: "/dev/urandom")
entropy_source = "/dev/urandom"
# number of guest memory slots, (default: 1)
mem_slots = 1
# guest physical address offset, (default: 0)
mem_offset = 0
# path for memory backend, (default: "")
memory_path = ""
# file path for memory backend, (default: "")
file_backend_mem_path = ""
# preallocate guest memory, (default: false)
mem_prealloc = false
# enable hugepages support, (default: false)
hugepages = false
# enable vhost-user persistent storage, (default: false)
enable_vhost_user_store = false
# enable swap in guest, (default: false)
enable_swap = false
# virtiofs daemon path, (default: "/usr/bin/virtiofsd")
virtiofs_daemon_path = "/usr/bin/virtiofsd"
# virtiofs cache mode, (default: "always")
virtiofs_cache = "always"
# extra arguments for virtiofs daemon, (default: [])
virtiofs_extra_args = []
# virtiofs cache size in MB, (default: 1024)
virtiofs_cache_size = 1024
# the msize for 9p shares
msize_9p = 8192
# enable direct I/O bypassing host cache for 9p, (default: false)
virtio_9p_direct_io = false
# 9p multi-device support, (default: "")
virtio_9p_multidevs = ""
# enable dedicated I/O threads, (default: false)
enable_iothreads = false
# block device driver, (default: "VirtioBlk")
block_device_driver = "VirtioBlk"
# disable NVDIMM support, (default: true)
disable_nvdimm = true
# shared filesystem type, (default: "Virtio9P")
share_fs = "Virtio9P"
# use vsock for host-guest communication, (default: true)
use_vsock = true
```

### Start containerd process

```bash
# TODO: create a containerd systemd service with ENABLE_CRI_SANDBOXES env
$ ENABLE_CRI_SANDBOXES=1 ./bin/containerd
```

### Run kuasar-vmm service

```bash
$ systemctl start kuasar-vmm
$ systemctl status kuasar-vmm
```

### Run pod sandbox with config file

```bash
# Create pod sandbox
$ cat podsandbox.yaml 
metadata:
  attempt: 1
  name: busybox-sandbox2
  namespace: default
  uid: hdishd83djaidwnduwk28bcsc
log_directory: /tmp
linux:
  namespaces:
    options: {}
    
$ crictl runp --runtime=kuasar-vmm podsandbox.yaml
```

### Create and start container in the pod sandbox with config file

```bash
$ cat container.yaml
metadata:
  name: busybox1
image:
  image: docker.io/library/busybox:latest
command:
- top
log_path: busybox.0.log
no pivot: true

$ crictl create <pod-id> container.yaml podsandbox.yaml
$ crictl start <container-id>
```


