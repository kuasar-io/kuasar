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

# PARALLEL is the number of container
PARALLEL=50
workdir=$(dirname "$(pwd)")
RAW=${workdir}/data/raw
TIME_DAT=${RAW}/boot-parallel-${PARALLEL}-kata-time.dat
mkdir -p $workdir/json/container
mkdir -p $workdir/json/pod

#Function to get the interval time(ms)
function getTiming(){
    start=$1
    end=$2

    start_s=$(echo $start | cut -d '.' -f 1)
    start_ns=$(echo $start | cut -d '.' -f 2)
    end_s=$(echo $end | cut -d '.' -f 1)
    end_ns=$(echo $end | cut -d '.' -f 2)


    time=$(( ( 10#$end_s - 10#$start_s ) * 1000 + ( 10#$end_ns / 1000000 - 10#$start_ns / 1000000 ) ))

    echo "$time"
}

once_test(){

# Kill all pods to prevent interference with testing
crictl rm -f -a
crictl rmp -f -a

# Create $PARALLEL container.json and pod.json
for((i=0;i<$PARALLEL;i++))
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

# Start timing
start_time=$(date +%s.%N)

for((i=0;i<$PARALLEL;i++))
do
    crictl run -r kata --no-pull $workdir/json/container/container_$i.json $workdir/json/pod/pod_$i.json &
done

end_time=$(date +%s.%N)

# Wait for all the containers to finish starting
a=`crictl ps | grep testcontainer | wc -l`
while [ $a -ne $PARALLEL ];
do
a=`crictl ps | grep testcontainer | wc -l`
end_time=$(date +%s.%N)
if [ $(getTiming $start_time $end_time) -gt 6000 ]; then
break
fi
done

# End timing
boot_time=$(getTiming $start_time $end_time)

if [ ${boot_time} -lt 6000 ]; then
echo "BootTime: ${boot_time}ms"

# Output to the corresponding file
echo "${boot_time}" >> ${TIME_DAT}
fi

}

while [ true ];
do
once_test
if [ $(cat ${TIME_DAT} | wc -l) -gt 1000 ]; then
break
fi
sleep 2s
done
