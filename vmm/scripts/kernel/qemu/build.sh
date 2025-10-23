#!/bin/bash
# Copyright 2023 Kairus.Zhang.
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
set -x

readonly version=${1:-6.12.8}
readonly arch=${2:-x86_64}
readonly base_dir="$(dirname "$(readlink -f "$0")")"
readonly work_dir="/tmp/linux-qemu-build"
readonly target_dir="${base_dir}"

mkdir -p "${target_dir}"

case "${arch}" in
    x86_64)
        defconfig="x86_64_defconfig"
        kernel_path="arch/x86/boot/compressed/vmlinux.bin"
        ;;
    aarch64)
        defconfig="defconfig"
        kernel_path="arch/arm64/boot/Image"
        ;;
    *)
        exit 1
        ;;
esac

sudo apt-get update -y
sudo apt-get install -y \
    build-essential \
    libncurses-dev \
    bison \
    flex \
    libssl-dev \
    libelf-dev \
    wget \
    git

rm -rf "${work_dir}"
mkdir -p "${work_dir}"

git clone --depth 1 --single-branch -b "v${version}" \
https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git"${work_dir}"

cd "${work_dir}"
make "${defconfig}"
make -j "$(nproc)" bzImage
cp "${work_dir}/${kernel_path}" "${target_dir}/vmlinux.bin"
