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

# Prepare for the Pod and Container config file.
touch pod.json container.json
current_timestamp=$(date +%s)
cat > pod.json <<EOF
{
    "metadata": {
        "name": "test-sandbox$current_timestamp",
        "namespace": "default",
        "uid": "JsHpiD0hA6EZ0iGy"
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
        "name": "ubuntu",
        "namespace": "default"
    },
    "image": {
      "image": "ubuntu:latest"
    },
    "command": [
       "/bin/sh", "-c", "while true; do echo \`date\`; sleep 1; done"
    ],
    "log_path":"ubuntu.log",
    "linux": {
        "security_context": {
            "apparmor_profile": "unconfined",
            "namespace_options": {
                "network": 2,
                "pid": 1
            }
        }
    }
}
EOF

# Run a container, default runtime is "kuasar-vmm".
runtime=${1:-kuasar-vmm}
crictl -r unix:///run/containerd/containerd.sock run --runtime="$runtime" container.json pod.json
rm -f container.json pod.json
