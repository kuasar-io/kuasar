#!/bin/bash
# Copyright 2023 The Kuasar Authors.
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

kernel_merge_script="scripts/kconfig/merge_config.sh"
kernel_merge_options=("-r" "-n")
faild_merge_keyword="not in final .config"

print_usage() {
    echo "Usage: $0 [options]
    --help, -h             print the usage
    --arch                 specify the hardware architecture: aarch64/x86_64
    --kernel-type          specify the target kernel type: micro/mini
    --kernel-dir           specify the kernel source directory
    --kernel-conf-dir      specify the kernel tailor conf directory"
}

merge_kernel_fragments() {
    local tailor_conf_file="$1"

    if [ ! -f "$tailor_conf_file" ]; then
        echo "Tailor conf file does not exist: $tailor_conf_file"
        return 1
    fi

    local kernel_fragments=$(sed "s#^#${kernel_conf_dir}/#" "${tailor_conf_file}" | tr '\n' ' ')
    read -a kernel_fragments_arr <<<"${kernel_fragments}"
    # need to change the pwd to kernel directory to do merge kernel fragments operation
    cd ${kernel_dir}
    local results=$(bash "${kernel_dir}/${kernel_merge_script}" "${kernel_merge_options[@]}" "${kernel_fragments_arr[@]}")

    if [[ "${results}" == *"${faild_merge_keyword}"* ]]; then
        echo "Error: failed to merge kernel fragments with ${tailor_conf_file} configuration."
        echo "The kernel configs which are not present in the final .config file:  "
        echo "${results}"
        return 1
    fi

    echo "Merge kernel fragments with ${tailor_conf_file} successfully."
    return 0
}

build_kernel() {
    cd ${kernel_dir}
    make -j $(nproc)
    if [ $? -ne 0 ]; then
        echo "Error: Failed to build kernel."
        return 1
    fi
    echo "Build kernel successfully."
    return 0
}

while [[ "$#" -gt 0 ]]; do
    case $1 in
    -h | --help)
        print_usage
        exit 0
        ;;
    --arch)
        arch="$2"
        shift
        ;;
    --kernel-type)
        kernel_type="$2"
        shift
        ;;
    --kernel-dir)
        kernel_dir="$2"
        shift
        ;;
    --kernel-conf-dir)
        kernel_conf_dir="$2"
        shift
        ;;
    *)
        echo "Unknown parameter passed: $1"
        print_usage
        exit 1
        ;;
    esac
    shift
done

if [ -z "$kernel_type" ] || [ -z "$arch" ] || [ -z "$kernel_dir" ] || [ -z "$kernel_conf_dir" ]; then
    print_usage
    exit 1
fi

echo "Arch: $arch"
echo "Kernel Type: $kernel_type"
echo "Kernel Dir: $kernel_dir"
echo "Kernel Conf Dir: $kernel_conf_dir"

# select the tailor conf file by vm type and cpu architecture
tailor_conf_file="${kernel_conf_dir}/${kernel_type}-kernel-${arch}.list"

cd ${kernel_conf_dir}
merge_kernel_fragments $tailor_conf_file
if [ $? -ne 0 ]; then
    exit 1
fi

build_kernel
