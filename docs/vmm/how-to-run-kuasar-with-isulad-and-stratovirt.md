
## Requirement

- Prepare a new aarch64 architecture physical server
- *Optional* - Recommend to install openEuler 23.03 version OS in the server
  - openEuler 23.03 version OS image download link: https://repo.openeuler.org/openEuler-23.03/ISO/

> Note: All of the following commands need to run with root privilege.

## Build and install StratoVirt

- If you use openEuler 23.03 OS in your physical server, you can directly install the newest StratoVirt package by yum.
  ```bash
  $ yum install stratovirt
  ```

- If you use another Linux distribution OS, you can build the stratovirt from the source and install it: [Build StratoVirt](https://gitee.com/openeuler/stratovirt/blob/master/README.md#build-stratovirt)

- After you build or install the stratovirt package, you can find the following important binary file in your sever:
  ```bash
  # stratovirt hypervisor binary file
  $ ls /usr/bin/stratovirt 
  /usr/bin/stratovirt

  # stratovirt related virtiofs daemon binary file
  $ ls /usr/bin/vhost_user_fs 
  /usr/bin/vhost_user_fs
  ```

## Build and configure kuasar vmm-sandboxer with stratovirt hypervisor

### Build requirement

Kuasar use  `docker` or `containerd` container engine to build guest os initrd image, so you need to **make sure `docker` or `containerd` is correctly installed and can pull the image from the dockerhub registries**. 

> Tips: `make vmm` build command will download the Rust and Golang packages from the internet, so you need to provide the `http_proxy` and `https_proxy` environments for the `make all` command.
>
> If a self-signed certificate is used in the `make all` build command execution environment, you may encounter SSL issues with downloading resources from https URL failed. Therefore, you need to provide a CA-signed certificate and copy it into the root directory of the Kuasar project, then rename it as "proxy.crt". In this way, our build script will use the "proxy.crt" certificate to access the https URLs of Rust and Golang installation packages.

### Build and install kuasar vmm-sandboxer

```bash
# build kuasar vmm-sandboxer with stratovirt hypervisor
$ HYPERVISOR=stratovirt make vmm

# install kuasar vmm-sandboxer
$ HYPERVISOR=stratovirt make install-vmm
```

After installation, you will find the required files in the specified path
```bash
# 1. vmm-sandboxer binary
/usr/local/bin/vmm-sandboxer

# 2. kernel binary
/var/lib/kuasar/vmlinux.bin

# 3. initrd image file
/var/lib/kuasar/kuasar.initrd

# 4. stratovirt config toml file
/var/lib/kuasar/config_stratovirt.toml
```

## Build and configure iSulad

[iSulad](https://gitee.com/openeuler/iSulad) supports Kuasar with its master branch at the moment. For building iSulad from scratch, please refer to [iSulad build guide](https://gitee.com/openeuler/iSulad/blob/master/docs/build_docs/guide/build_guide.md). Here we only emphasize the difference of the building steps.

### Build LCR

```bash
$ git clone https://gitee.com/openeuler/lcr.git
$ cd lcr
$ mkdir build
$ cd build
$ sudo -E cmake ..
$ sudo -E make -j $(nproc)
$ sudo -E make install
```

### Build iSulad

```bash
$ git clone https://gitee.com/openeuler/iSulad.git
$ cd iSulad
$ mkdir build
$ cd build
$ sudo -E cmake .. -DENABLE_CRI_API_V1=ON -DENABLE_SHIM_V2=ON -DENABLE_SANDBOXER=ON
$ sudo make -j
$ sudo -E make install
```

### Configure iSulad
Add the following configuration in the iSulad configuration file `/etc/isulad/daemon.json` 
```json
...
    "default-sandboxer": "vmm",
    "cri-sandboxers": {
        "vmm": {
            "name": "vmm",
            "address": "/run/vmm-sandboxer.sock"
        }
    },
    "cri-runtimes": {
        "vmm": "io.containerd.vmm.v1"
    },
...
```

## Build and configure Containerd

### Build containerd

Sine some code have not been merged into the upstream containerd community, so you need to manually compile the containerd source code in the [kuasar-io/containerd](https://github.com/kuasar-io/containerd.git). 

git clone the codes of containerd fork version from kuasar repository.
```bash
$ git clone https://github.com/kuasar-io/containerd.git
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

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.vmm]
  runtime_type = "io.containerd.kuasar.v1"
  sandboxer = "vmm"
  io_type = "hvsock"
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
# isulad container engine configuraiton
runtime-endpoint: unix:///var/run/isulad.sock
image-endpoint: unix:///var/run/isulad.sock

# containerd container engine configuration
# runtime-endpoint: unix:///var/run/containerd/containerd.sock
# image-endpoint: unix:///var/run/containerd/containerd.sock
timeout: 10
```

## Run pod and container with crictl

### Configure config_stratovirt.toml file

The default config file `/var/lib/kuasar/config_stratovirt.toml` for stratovirt vmm-sandboxer:
```toml
  [sandbox]
  # set kuasar log level, (default: info)
  log_level = "info"

  [hypervisor]
  # set stratovirt binary path, (default: stratovirt)
  path = "/usr/bin/stratovirt"
  # set the type of the analog chip, "virt" for ARM architecture and "q35" for x86 architecture, (default: virt)
  machine_type = "virt,mem-share=on"
  # set guest kernel path, (default: /var/lib/kuasar/vmlinux.bin)
  kernel_path = "/var/lib/kuasar/vmlinux.bin"
  # set guest image path, (default: "")
  image_path = ""
  # set guest initrd path, select either image or image, (default: "")
  initrd_path = "/var/lib/kuasar/kuasar.initrd"
  # set parameters of the guest kernel, (default: "")
  kernel_params = "task.log_level=debug task.sharefs_type=virtiofs"
  # set number of vcpus for each sandbox, (default: 1)
  vcpus = 1
  # set memory size for each sandbox, (default: 1024)
  memory_in_mb = 1024
  # set the drivers of block devices, (default: virtio-blk)
  block_device_driver = "virtio-blk"       
  # set whether to enable debug mode, (default: false)
  debug = true
  # enable VM RAM pre-allocation, (default: false)
  enable_mem_prealloc = false
  
  [hypervisor.virtiofsd_conf]
  # set vhost_user_fs path, (default: /usr/bin/vhost_user_fs)
  path = "/usr/bin/vhost_user_fs"
```

### Start containerd process

```bash
# TODO: create a containerd systemd service with ENABLE_CRI_SANDBOXES env
$ ENABLE_CRI_SANDBOXES=1 ./bin/containerd
```

### Run kuasar-vmm service

```bash
$ systemctl start kuasar-vmm
```

### Run pod sandbox with config file

```bash
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

$ crictl runp --runtime=vmm podsandbox.yaml
5cbcf744949d8500e7159d6bd1e3894211f475549c0be15d9c60d3c502c7ede3
```

> Tips: `--runtime=vmm` indicates that containerd needs to use vmm-sandboxer runtime to run a pod sandbox

- List pod sandboxes and check the sandbox is in Ready state:
```bash
$ crictl pods
POD ID              CREATED              STATE               NAME                NAMESPACE           ATTEMPT
5cbcf744949d8       About a minute ago   Ready               busybox-sandbox2    default             1
```

### Create and start container in the pod sandbox with config file

Create a container in the podsandbox
```bash
$ cat pod-container.yaml
metadata:
  name: busybox1
image:
  image: docker.io/library/busybox:latest
command:
- top
log_path: busybox.0.log
no pivot: true

$ crictl create 5cbcf744949d8500e7159d6bd1e3894211f475549c0be15d9c60d3c502c7ede3 pod-container.yaml podsandbox.yaml
c11df540f913e57d1e28372334c028fd6550a2ba73208a3991fbcdb421804a50
```

List containers and check the container is in Created state:
```bash
$ crictl ps -a
CONTAINER           IMAGE                              CREATED             STATE               NAME                ATTEMPT             POD ID
c11df540f913e       docker.io/library/busybox:latest   15 seconds ago      Created             busybox1            0                   5cbcf744949d8
```

Start container in the podsandbox
```bash
$ crictl start c11df540f913e57d1e28372334c028fd6550a2ba73208a3991fbcdb421804a50

$ crictl ps
CONTAINER           IMAGE                              CREATED             STATE               NAME                ATTEMPT             POD ID
c11df540f913e       docker.io/library/busybox:latest   2 minutes ago       Running             busybox1            0                   5cbcf744949d8
```

### Test Network Connectivity

Get the `vsock guest-cid` from stratovirt vm process
```bash
$ ps -ef | grep stratovirt | grep 5cbcf744949d8 
/usr/bin/stratovirt -name sandbox-5cbcf744949d8500e7159d6bd1e3894211f475549c0be15d9c60d3c502c7ede3 ...
-device vhost-vsock-pci,id=vsock-395568061,guest-cid=395568061,bus=pcie.0,addr=0x3,vhostfd=3 
...
```

Enter the guest os debug console shell environment:
```bash
# ncat --vsock <guest-cid> <debug-console>
# enter the guest os debug console shell
$ ncat --vsock 395568061 1025

# now in the guest os console shell
bash-5.1# busybox ip addr show
busybox ip addr show
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue qlen 1000
    link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00
    inet 127.0.0.1/8 scope host lo
       valid_lft forever preferred_lft forever
    inet6 ::1/128 scope host 
       valid_lft forever preferred_lft forever
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc pfifo_fast qlen 1000
    link/ether 42:e2:92:d4:39:9f brd ff:ff:ff:ff:ff:ff
    inet 172.19.0.240/24 brd 172.19.0.255 scope global eth0
       valid_lft forever preferred_lft forever
    inet6 fe80::40e2:92ff:fed4:399f/64 scope link 
       valid_lft forever preferred_lft forever

# 172.19.0.1 is the gateway ip in the host
bash-5.1# busybox ping 172.19.0.1
busybox ping 172.19.0.1
PING 172.19.0.1 (172.19.0.1): 56 data bytes
64 bytes from 172.19.0.1: seq=0 ttl=64 time=0.618 ms
64 bytes from 172.19.0.1: seq=1 ttl=64 time=0.116 ms
64 bytes from 172.19.0.1: seq=2 ttl=64 time=0.152 ms
```