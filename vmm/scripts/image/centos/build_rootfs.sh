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

golang_version="1.20.5"
cert_file_path="/kuasar/proxy.crt"
ARCH=$(uname -m)

current_dir=$(dirname "$(realpath "$0")")
source $current_dir/../common.sh

main() {
    local rootfs_dir=${ROOTFS_DIR:-/tmp/kuasar-rootfs}
    local repo_dir=${REPO_DIR:-/kuasar}
    make_vmm_task ${repo_dir}
    install_golang
    build_runc ${repo_dir}
    create_tmp_rootfs ${rootfs_dir}

    # copy init into rootfs
    cp ${repo_dir}/bin/vmm-task ${rootfs_dir}/sbin/init
    ln -s /sbin/init ${rootfs_dir}/init
    # copy runc binary
    cp ${repo_dir}/bin/runc ${rootfs_dir}/bin/runc
    # copy glibc-devel glibc
    cp /lib64/libnss_dns* ${rootfs_dir}/lib64
    cp /lib64/libnss_files* ${rootfs_dir}/lib64
    cp /lib64/libresolv* ${rootfs_dir}/lib64
    # /etc/protocols is for nfsv3
    cp /etc/protocols ${rootfs_dir}/lib64

    install_and_copy_rpm ${current_dir}/rpm.list ${rootfs_dir}
    copy_binaries ${current_dir}/binaries.list ${rootfs_dir}
    copy_libs ${current_dir}/binaries.list ${rootfs_dir}
    echo "Succeed building rootfs"
}

main "$@"
