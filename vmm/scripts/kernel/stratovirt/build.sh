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

readonly branch="openEuler-22.03-LTS"
readonly base_dir="$(dirname $(readlink -f $0))"

# clone kernel source from openEuler/kenel repo
# if openEuler/kernel repo already exist in the /tmp/openEuler-kernel dir, doesn't delete it
# check openEuler/kernel exist or not
cd /tmp/openEuler-kernel && git remote show origin | grep "https://gitee.com/openeuler/kernel.git"
if [ "$?" == "1" ];then
    git clone --depth 1 https://gitee.com/openeuler/kernel.git -b ${branch} /tmp/openEuler-kernel
fi

pushd /tmp/openEuler-kernel
# aarch64 kernel config
cp ${base_dir}/kuasar-openeuler-kernel-aarch64.config .config
make -j `nproc`
popd # pushd /tmp/openEuler-kernel

cp /tmp/openEuler-kernel/arch/arm64/boot/Image ${base_dir}/vmlinux.bin