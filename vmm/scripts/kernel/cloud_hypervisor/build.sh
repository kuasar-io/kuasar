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

readonly version=${1:-6.1.6}
readonly base_dir="$(dirname $(readlink -f $0))"

sudo apt-get update
sudo apt-get install -y libelf-dev elfutils

# clone kernel from cloud-hypervisor github
rm -rf /tmp/linux-cloud-hypervisor
git clone --depth 1 https://github.com/cloud-hypervisor/linux.git -b ch-${version} /tmp/linux-cloud-hypervisor
pushd /tmp/linux-cloud-hypervisor
wget --no-check-certificate https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/main/resources/linux-config-x86_64
# TODO support arm
# wget https://raw.githubusercontent.com/cloud-hypervisor/cloud-hypervisor/main/resources/linux-config-aarch64
cp linux-config-x86_64 .config  # x86-64
# TODO support arm
# cp linux-config-aarch64 .config # AArch64
KCFLAGS="-Wa,-mx86-used-note=no" make bzImage -j `nproc`
make -j `nproc`
popd # pushd /tmp/linux-cloud-hypervisor

cp /tmp/linux-cloud-hypervisor/arch/x86/boot/compressed/vmlinux.bin ${base_dir}/vmlinux.bin
