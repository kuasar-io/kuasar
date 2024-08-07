# Containerd in Kuasar

Kuasar has make some changes on official containerd v1.7.0 based on commit:`1f236dc57aff44eafd95b3def26683a235b97241`.

## Building and installing containerd

- Please note that for compatibility with Containerd, it is recommended to use Go version 1.19 or later.

- `git` clone the codes of containerd fork version from kuasar repository.

```bash
git clone -b v0.2.0-kuasar https://github.com/kuasar-io/containerd.git
cd containerd
make
make install
```

## Configure containerd

Refer to the following configuration to modify the configuration file, default path is `/etc/containerd/config.toml`.

**Important!!!**: AppArmor feature is not support now, you need update `disable_apparmor = true` in the config file.

+ For vmm:

```toml
[proxy_plugins.vmm]
  type = "sandbox"
  address = "/run/vmm-sandboxer.sock"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kuasar-vmm]
  runtime_type = "io.containerd.kuasar-vmm.v1"
  sandboxer = "vmm"
  io_type = "hvsock"
```

+ For quark:

```toml
[proxy_plugins.quark]
  type = "sandbox"
  address = "/run/quark-sandboxer.sock"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kuasar-quark]
  runtime_type = "io.containerd.kuasar-quark.v1"
  sandboxer = "quark"
```

+ For wasm:

```toml
[proxy_plugins.wasm]
  type = "sandbox"
  address = "/run/wasm-sandboxer.sock"
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kuasar-wasm]
  runtime_type = "io.containerd.kuasar-wasm.v1"
  sandboxer = "wasm"
```

## Run containerd

To start containerd, run `ENABLE_CRI_SANDBOXES=1 containerd`

In order to use the containerd Sandbox API, the containerd daemon should be started with the environment variable `ENABLE_CRI_SANDBOXES=1`.