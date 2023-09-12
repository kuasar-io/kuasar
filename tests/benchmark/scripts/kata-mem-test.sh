#! /bin/bash
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

# $num is the number of pod
num=50
workdir=$(dirname $(cd $(dirname $0); pwd))
mkdir -p $workdir/json/container
mkdir -p $workdir/json/pod

# Kill all pods to prevent interference with testing
crictl rm -f -a
crictl rmp -f -a
rm -f mem

for((i=0;i<$num;i++))
do
   cat > $workdir/json/container/container_$i.json << EOF
{
        "metadata": {
             "name": "testcontainer"
        },
        "image": {
                "image": "docker.io/library/ubuntu:latest"
        },
        "command": [
                "/bin/sh", "-c", "sleep 1d"
        ],
        "log_path": "container.log",
        "linux": {
                "resources": {},
                "security_context": {
                "capabilities": {},
                "namespace_options": {
                "network": 2,
                "pid": 1
                }
        }
    }
}
EOF

    cat > $workdir/json/pod/pod_$i.json <<EOF
{
        "metadata": {
                "name": "testpod$i",
                "namespace": "docker2cric"
        },
        "log_directory": "/tmp",
        "dns_config": {},
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
done

for((i=0;i<$num;i++))
do
    crictl run -r kata --no-pull $workdir/json/container/container_$i.json $workdir/json/pod/pod_$i.json &
done

# Wait for all the containers to finish starting
a=`crictl ps | grep testcontainer | wc -l`
while [ $a -ne $(($num+1)) ];
do
a=`crictl ps | grep testcontainer | wc -l`
done

# Get pids of all containers
pids=$(ps -ef | grep -v grep | grep -i containerd-shim-kata-v2 | awk '{print$2}')

# Get Pss of each container
for i in ${pids}
do
cat /proc/$i/smaps_rollup | grep "Pss" >> mem
done

pss_array=$(cat mem | grep -w Pss | awk '{print$2}')

pss_total=0
for i in $pss_array
do
let pss_total=$pss_total+$i
done

echo "PssTotal: ${pss_total}kB" >> mem
cat mem | tail -n 1
