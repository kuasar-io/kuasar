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

readonly version=${1:-6.12}
readonly base_dir="$(dirname "$(readlink -f "$0")")"
readonly make_target="bzImage"

sudo apt-get update

# clone kernel from Linus-kernel github
rm -rf /tmp/linux-qemu
git clone --depth 1 --single-branch -b "v${version}" GitHub - torvalds/linux: Linux kernel source tree /tmp/linux-qemu
pushd /tmp/linux-qemu

make defconfig
./scripts/config -e BLK_DEV_INITRD -e SERIAL_8250_CONSOLE -e DEVTMPFS -e PROC_FS -e SYSFS -e VIRTIO -e VIRTIO_BLK -e VIRTIO_NET -e VIRTIO_CONSOLE -e 8139TOO -e BINFMT_ELF
make olddefconfig

# Do native build of the x86-64 kernel
make -j "$(nproc)" "${make_target}"
cp  /tmp/linux-qemu/arch/x86/boot/bzImage ${base_dir}/vmlinux.bin