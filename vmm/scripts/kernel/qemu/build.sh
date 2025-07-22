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

# 定义参数和默认值
readonly version=${1:-6.12.8}
readonly arch=${2:-x86_64}
readonly base_dir="$(dirname "$(readlink -f "$0")")"
readonly work_dir="/tmp/linux-qemu-build"
readonly target_dir="${base_dir}"

# 创建目标目录
mkdir -p "${target_dir}"

# 定义架构相关的配置
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
        echo "错误：不支持的架构 ${arch}"
        exit 1
        ;;
esac

# 安装编译依赖
echo "安装编译依赖..."
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

# 清理并创建工作目录
echo "准备工作目录..."
rm -rf "${work_dir}"
mkdir -p "${work_dir}"

# 克隆内核源码
echo "克隆 Linux 内核源码 (版本: ${version}, 架构: ${arch})..."
git clone --depth 1 --single-branch -b "v${version}" \
https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git"${work_dir}"

# 进入内核源码目录
cd "${work_dir}"

# 配置内核
echo "配置内核..."
make "${defconfig}"

# 自定义内核配置（可选）
# 取消注释以下行以启用交互式配置
# make menuconfig

# 编译内核
echo "开始编译内核..."
make -j "$(nproc)" bzImage

# 复制编译结果
echo "复制内核二进制文件..."
cp "${work_dir}/${kernel_path}" "${target_dir}/vmlinux.bin"

# 显示成功信息
echo "======================================================================"
echo "内核编译成功！"
echo "版本: ${version}"
echo "架构: ${arch}"
echo "内核位置: ${target_dir}/vmlinux.bin"
echo "======================================================================"
