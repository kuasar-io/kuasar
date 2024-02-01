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

[ -n "${DEBUG}" ] && set -x

golang_version="1.20.5"
cert_file_path="/kuasar/proxy.crt"
ARCH=$(uname -m)

make_vmm_task() {
    local repo_dir="$1"

    # install cmake3 because some rust modules depends on it
    # centos 7 install cmake 2.8 by default, has to add epel repo to install cmake3
    . /etc/os-release
    if [ ${VERSION_ID} -le 7 ]; then
        yum install -y epel-release
        yum install -y cmake3 make gcc gcc-c++ wget
        rm -f /usr/bin/cmake
        ln -s /usr/bin/cmake3 /usr/bin/cmake
    else
        # CentOS Linux 8 had reached the End Of Life (EOL) on December 31st, 2021. It means that CentOS 8 will no longer receive development resources from the official CentOS project. After Dec 31st, 2021, if you need to update your CentOS, you need to change the mirrors to vault.centos.org where they will be archived permanently. Alternatively, you may want to upgrade to CentOS Stream.
        sed -i 's/mirrorlist/#mirrorlist/g' /etc/yum.repos.d/CentOS-*
        sed -i 's|#baseurl=http://mirror.centos.org|baseurl=http://vault.centos.org|g' /etc/yum.repos.d/CentOS-*
        yum clean all
        yum install cmake make gcc-c++ wget
    fi

    # update cert file under internal proxy scenario
    if [ -f "${cert_file_path}" ]; then
        cp ${cert_file_path} /etc/pki/ca-trust/source/anchors/
        update-ca-trust extract
    fi

    # install rust to compile vmm-task
    pushd ${repo_dir}

    source vmm/scripts/image/install_rust.sh

    make bin/vmm-task ARCH=${ARCH}
    popd
}

install_golang() {
    pushd /home/
    arch_name=""
    if [ "${ARCH}" == "aarch64" ]; then
        arch_name="arm64"
    elif [ "${ARCH}" == "x86_64" ]; then
        arch_name="amd64"
    else
        echo "Unsupported arch: ${ARCH}"
        exit 1
    fi
    rm -f go${golang_version}.linux-${arch_name}.tar.gz
    wget -q "https://go.dev/dl/go${golang_version}.linux-${arch_name}.tar.gz"
    rm -rf /usr/local/go && tar -C /usr/local -xzf "go${golang_version}.linux-${arch_name}.tar.gz"
    echo "export PATH=$PATH:/usr/local/go/bin" >>/etc/profile
    source /etc/profile
    go version
    popd
}

build_runc() {
    local repo_dir="$1"
    mkdir -p /tmp/gopath
    GOPATH=/tmp/gopath go install github.com/opencontainers/runc@v1.1.6
    cp /tmp/gopath/bin/runc ${repo_dir}/bin/
}

create_tmp_rootfs() {
    local rootfs_dir="$1"
    rm -rf ${rootfs_dir}/*
    mkdir -p ${rootfs_dir}/lib \
        ${rootfs_dir}/lib64 \
        ${rootfs_dir}/lib/modules

    mkdir -m 0755 -p ${rootfs_dir}/dev \
        ${rootfs_dir}/sys \
        ${rootfs_dir}/sbin \
        ${rootfs_dir}/bin \
        ${rootfs_dir}/tmp \
        ${rootfs_dir}/proc \
        ${rootfs_dir}/etc \
        ${rootfs_dir}/run \
        ${rootfs_dir}/var

    ln -s ../run ${rootfs_dir}/var/run
    touch ${rootfs_dir}/etc/resolv.conf
}

install_and_copy_rpm() {
    set +e
    local rpm_list_file="$1"
    local rootfs_dir="$2"
    cat ${rpm_list_file} | while read rpm; do
        if [ "${rpm:0:1}" != "#" ]; then
            rpm -ql $rpm >/dev/null 2>&1
            if [ $? -ne 0 ]; then
                yum install -y $rpm >/dev/null 2>&1
                if [ $? -ne 0 ]; then
                    echo "Can not install $rpm by yum"
                    exit 1
                fi
            fi
            array=($(rpm -ql $rpm | grep -v "share" | grep -v ".build-id"))
            for file in ${array[@]}; do
                source=$file
                dts_file=${rootfs_dir}$file
                dts_folder=${dts_file%/*}
                if [ ! -d "$dts_folder" ]; then
                    mkdir -p $dts_folder
                fi
                cp -r -f -d $source $dts_folder
            done
        fi
    done
    set -e
}

copy_binaries() {
    local binaries_list_file="$1"
    local rootfs_dir="$2"
    cat $binaries_list_file | while read line; do
        if [ -n "${line}" ] && [ "${line:0:1}" != "#" ]; then
            source=$(echo "${line}" | awk '{print $1}')
            des=$(echo "${line}" | awk '{print $2}')
            if [ ! -d "$des" ]; then
                mkdir -p $des
            fi
            cp -r -d $source ${rootfs_dir}/$des
        fi
    done
}

copy_libs() {
    local binaries_list_file="$1"
    local rootfs_dir="$2"
    binaries_list=$(cat $binaries_list_file | grep -v "#" | awk '{print $1}')
    binaries_list=(${binaries_list[@]} ${rootfs_dir}/sbin/init)
    for bin in ${binaries_list[@]}; do
        ldd ${bin} | while read line; do
            arr=(${line// / })

            for lib in ${arr[@]}; do
                echo $lib
                if [ "${lib:0:1}" = "/" ]; then
                    dir=${rootfs_dir}/$(dirname $lib)
                    mkdir -p "${dir}"
                    cp -f $lib $dir
                fi
            done
        done
    done
}

main() {
    local rootfs_dir=${ROOTFS_DIR:-/tmp/kuasar-rootfs}
    local repo_dir=${REPO_DIR:-/kuasar}
    local current_dir="$(dirname $(readlink -f $0))"
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
