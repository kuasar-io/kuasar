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

set -e

RUNWASI_PATH="/tmp/runwasi"

build_wasm_image() {
  rustup target add wasm32-wasi
  if [ -e $RUNWASI_PATH ]; then
    rm -rf $RUNWASI_PATH
  fi
  git clone https://github.com/containerd/runwasi.git $RUNWASI_PATH
  pushd $RUNWASI_PATH
  make CONTAINERD_NAMESPACE="k8s.io" load
  popd
  rm -rf $RUNWASI_PATH
}

# Build the wasi-demo-app image and import it to containerd if not exist.
crictl -r unix:///run/containerd/containerd.sock images -v | grep ghcr.io/containerd/runwasi/wasi-demo-app:latest >> /dev/null || build_wasm_image

# Prepare for the wasm Pod and Container config file.
touch pod.json container.json
current_timestamp=$(date +%s)
cat > pod.json <<EOF
{
    "metadata": {
        "name": "test-sandbox$current_timestamp",
        "namespace": "default",
        "uid": "YFaKnzzBbsbYmw6w"
    },
    "log_directory": "/tmp",
    "linux": {
        "security_context": {
            "namespace_options": {
                "network": 2,
                "pid": 1
            }
        }
    }
}
EOF
cat > container.json <<EOF
{
    "metadata": {
        "name": "wasm",
        "namespace": "default"
    },
    "image": {
      "image": "ghcr.io/containerd/runwasi/wasi-demo-app:latest"
    },
    "log_path":"wasm.log",
    "linux": {
        "security_context": {
            "namespace_options": {
                "network": 2,
                "pid": 1
            }
        }
    }
}
EOF

# Run a wasm container
crictl -r unix:///run/containerd/containerd.sock run --runtime="wasm" --no-pull container.json pod.json
rm -f container.json pod.json
