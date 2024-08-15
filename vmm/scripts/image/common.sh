#!/bin/bash
# Copyright 2024 The Kuasar Authors.
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

make_vmm_task() {
    local repo_dir="$1"

    yum install -y cmake make gcc-c++ wget

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
                    continue
                fi
                rpm -ql $rpm >/dev/null 2>&1 || continue
            fi
            array=($(rpm -ql $rpm | grep -v "share" | grep -v ".build-id"))
            for file in ${array[@]}; do
                source=$file
                dst_file=${rootfs_dir}$file
                dst_folder=${dst_file%/*}
                if [ ! -d "$dst_folder" ] && [ ! -L "$dst_folder" ]; then
                    mkdir -p $dst_folder
                fi
                cp -r -f -d $source $dst_folder
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
