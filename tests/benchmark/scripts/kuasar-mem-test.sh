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

# Remove to prevent interference with testing       
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
        "annotations": {
                "com.github.containers.virtcontainers.sandbox_cpu": "1",
                "com.github.containers.virtcontainers.sandbox_mem": "2147483648"
        },
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
    crictl run -r kuasar --no-pull $workdir/json/container/container_$i.json $workdir/json/pod/pod_$i.json &
done

# Wait for all the containers to finish starting
a=`crictl ps | grep testcontainer | wc -l`
while [ $a -ne $(($num+1)) ];
do
a=`crictl ps | grep testcontainer | wc -l`
done

# Get pid of vmm-sandboxer
pid=$(ps -ef | grep -v grep | grep vmm-sandboxer | awk '{print$2}')

# Get Pss of vmm-sandboxer
cat /proc/$pid/smaps_rollup | grep "Pss" >> mem

pss_array=$(cat mem | grep -w Pss | awk '{print$2}')

pss_total=0
for i in $pss_array
do
let pss_total=$pss_total+$i
done

echo "PssTotal: ${pss_total}kB" >> mem
cat mem | tail -n 1
