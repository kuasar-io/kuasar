#!/bin/bash
# Copyright 2022 The Kuasar Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

git clone --depth 1 -b v0.2.0-kuasar https://github.com/kuasar-io/containerd.git "${tmp_dir}/containerd"
mkdir -p "${repo_root}/bin"
make -C "${tmp_dir}/containerd" bin/containerd
mv "${tmp_dir}/containerd/bin/containerd" "${repo_root}/bin/containerd"

tee "${repo_root}/bin/config.toml" > /dev/null <<EOF
version = 3

[plugins.'io.containerd.cri.v1.runtime']
disable_apparmor = true

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-runc]
runtime_type = "io.containerd.kuasar-runc.v1"
sandboxer = "runc"

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-vmm]
runtime_type = "io.containerd.kuasar-vmm.v1"
sandboxer = "vmm"
io_type = "streaming"

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-quark]
runtime_type = "io.containerd.kuasar-quark.v1"
sandboxer = "quark"

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kuasar-wasm]
runtime_type = "io.containerd.kuasar-wasm.v1"
sandboxer = "wasm"

[proxy_plugins.vmm]
type = "sandbox"
address = "/run/vmm-sandboxer.sock"

[proxy_plugins.quark]
type = "sandbox"
address = "/run/quark-sandboxer.sock"

[proxy_plugins.wasm]
type = "sandbox"
address = "/run/wasm-sandboxer.sock"

[proxy_plugins.runc]
type = "sandbox"
address = "/run/runc-sandboxer.sock"
EOF
