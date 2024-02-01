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

exit_flag=0
export IMAGE_NAME=${IMAGE_NAME:-"centos:7"}
export ROOTFS_DIR=${ROOTFS_DIR:-"/tmp/kuasar-rootfs"}
export CONTAINER_RUNTIME=${RUNTIME:-"containerd"}
CONTAINERD_NS=${CONTAINERD_NS:-"default"}

function fn_check_result() {
	if [ "$1" != "0" ]; then
		echo "FAILED: return $1 not as expected! ($2)"
		((exit_flag++))
	fi
}

# if build with ctr, then image name should have a default
# prefix of "docker.io/library/" if there is no one
if [ "${CONTAINER_RUNTIME}" == "containerd" ]; then
	image_parts=$(awk -F'/' '{print NF}' <<<"${IMAGE_NAME}")
	echo "image_parts ${image_parts}"
	if [[ ${image_parts} -le 1 ]]; then
		IMAGE_NAME=docker.io/library/${IMAGE_NAME}
	fi
fi

echo "build in ${IMAGE_NAME}"

if [ ! -n "${REPO_DIR}" ]; then
	current_dir=$(dirname "$(readlink -f "$0")")
	pushd ${current_dir}/../../../..
	REPO_DIR=$(pwd)
	popd
fi

# clean rootfs
rm -rf ${ROOTFS_DIR}
mkdir -p ${ROOTFS_DIR}

# build rootfs in container
case "${CONTAINER_RUNTIME}" in
containerd)
	image_count=$(ctr -n ${CONTAINERD_NS} images ls | grep ${IMAGE_NAME} | wc -l)
	if [[ ${image_count} -lt 1 ]]; then
		ctr -n ${CONTAINERD_NS} images pull ${IMAGE_NAME}
		fn_check_result $? "ctr pull image failed!"
	fi
	container_name="rootfs_builder-$(date +%s)"
	ctr -n ${CONTAINERD_NS} run \
		--rm \
		--net-host \
		--env http_proxy=${http_proxy} \
		--env https_proxy=${https_proxy} \
		--env ROOTFS_DIR=/tmp/kuasar-rootfs \
		--mount type=bind,src="${REPO_DIR}",dst=/kuasar,options=rbind:rw \
		--mount type=bind,src="${ROOTFS_DIR}",dst=/tmp/kuasar-rootfs,options=rbind:rw \
		${IMAGE_NAME} \
		${container_name} \
		bash /kuasar/vmm/scripts/image/centos/build_rootfs.sh
	fn_check_result $? "ctr run ${container_name} return error!"
	;;
docker)
	docker run \
		--rm \
		--env http_proxy=${http_proxy} \
		--env https_proxy=${https_proxy} \
		--env ROOTFS_DIR=/tmp/kuasar-rootfs \
		-v "${REPO_DIR}":/kuasar \
		-v "${ROOTFS_DIR}":"/tmp/kuasar-rootfs" \
		${IMAGE_NAME} \
		bash /kuasar/vmm/scripts/image/centos/build_rootfs.sh
	fn_check_result $? "docker run ${container_name} return error!"
	;;
isulad)
	isula run \
		--rm \
		--network host \
		--env http_proxy=${http_proxy} \
		--env https_proxy=${https_proxy} \
		--env ROOTFS_DIR=/tmp/kuasar-rootfs \
		-v "${REPO_DIR}":/kuasar \
		-v "${ROOTFS_DIR}":"/tmp/kuasar-rootfs" \
		${IMAGE_NAME} \
		bash /kuasar/vmm/scripts/image/centos/build_rootfs.sh
	fn_check_result $? "isula run ${container_name} return error!"
	;;
*)
	echo "${CONTAINER_RUNTIME} is not supported yet"
	((exit_flag++))
	;;
esac

if [ "${exit_flag}" != "0" ]; then
	rm -rf ${ROOTFS_DIR}
	exit ${exit_flag}
fi

case "$1" in
image)
	bash ${REPO_DIR}/vmm/scripts/image/build_image.sh
	fn_check_result $? "build image failed!"
	;;
initrd)
	initrd=${INITRD:-"/tmp/kuasar.initrd"}
	(cd ${ROOTFS_DIR} && find . | cpio -H newc -o | gzip -9) >"${initrd}"
	fn_check_result $? "build initrd failed!"
	;;
*)
	echo image type "$1" is not invalid
	((exit_flag++))
	;;
esac

exit ${exit_flag}
