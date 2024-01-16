#!/usr/bin/env bash
#
# Copyright (c) 2017-2019 Intel Corporation
#
# SPDX-License-Identifier: Apache-2.0

set -e

[ -n "${DEBUG}" ] && set -x

DOCKER_RUNTIME=${DOCKER_RUNTIME:-runc}

readonly script_name="${0##*/}"
readonly script_dir=$(dirname "$(readlink -f "$0")")

readonly ext4_format="ext4"
readonly xfs_format="xfs"

# ext4: percentage of the filesystem which may only be allocated by privileged processes.
readonly reserved_blocks_percentage=3

# Where the rootfs starts in MB
readonly rootfs_start=1

# Where the rootfs ends in MB
readonly rootfs_end=-1

# DAX header size
# * NVDIMM driver reads the device namespace information from nvdimm namespace (4K offset).
#   The MBR #1 + DAX metadata are saved in the first 2MB of the image.
readonly dax_header_sz=2

# DAX aligment
# * DAX huge pages [2]: 2MB alignment
# [2] - https://nvdimm.wiki.kernel.org/2mib_fs_dax
readonly dax_alignment=2

# The list of systemd units and files that are not needed in Kata Containers
readonly -a systemd_units=(
	"systemd-coredump@"
	"systemd-journald"
	"systemd-journald-dev-log"
	"systemd-journal-flush"
	"systemd-random-seed"
	"systemd-timesyncd"
	"systemd-tmpfiles-setup"
	"systemd-udevd"
	"systemd-udevd-control"
	"systemd-udevd-kernel"
	"systemd-udev-trigger"
	"systemd-update-utmp"
)

readonly -a systemd_files=(
	"systemd-bless-boot-generator"
	"systemd-fstab-generator"
	"systemd-getty-generator"
	"systemd-gpt-auto-generator"
	"systemd-tmpfiles-cleanup.timer"
)

# Set a default value
AGENT_INIT=${AGENT_INIT:-yes}

# Align image to (size in MB) according to different architecture.
case "$(uname -m)" in
	aarch64) readonly mem_boundary_mb=16 ;;
	*) readonly mem_boundary_mb=128 ;;
esac

error()
{
        local msg="$*"
        echo "ERROR: ${msg}" >&2
}

die()
{
        error "$*"
        exit 1
}

OK()
{
        local msg="$*"
        echo "[OK] ${msg}" >&2
}

info()
{
        local msg="$*"
        echo "INFO: ${msg}"
}

warning()
{
        local msg="$*"
        echo "WARNING: ${msg}"
}


usage() {
	cat <<EOT
Usage: ${script_name} [options] <rootfs-dir>
	This script will create a Kata Containers image file of
	an adequate size based on the <rootfs-dir> directory.

Options:
	-h Show this help
	-o path to generate image file ENV: IMAGE
	-r Free space of the root partition in MB ENV: ROOT_FREE_SPACE

Extra environment variables:
	AGENT_INIT: Use kuasar task as init process
	NSDAX_BIN:  Use to specify path to pre-compiled 'nsdax' tool.
	FS_TYPE:    Filesystem type to use. Only xfs and ext4 are supported.

Following diagram shows how the resulting image will look like

	.-----------.----------.---------------.-----------.
	| 0 - 512 B | 4 - 8 Kb |  2M - 2M+512B |    3M     |
	|-----------+----------+---------------+-----------+
	|   MBR #1  |   DAX    |    MBR #2     |  Rootfs   |
	'-----------'----------'---------------'-----------+
	      |          |      ^      |        ^
	      |          '-data-'      '--------'
	      |                                 |
	      '--------rootfs-partition---------'


MBR: Master boot record.
DAX: Metadata required by the NVDIMM driver to enable DAX in the guest [1][2] (struct nd_pfn_sb).
Rootfs: partition that contains the root filesystem (/usr, /bin, ect).

Kernels and hypervisors that support DAX/NVDIMM read the MBR #2, otherwise MBR #1 is read.

[1] - https://github.com/kata-containers/osbuilder/blob/master/image-builder/nsdax.gpl.c
[2] - https://github.com/torvalds/linux/blob/master/drivers/nvdimm/pfn.h

EOT
}

check_rootfs() {
	local rootfs="${1}"

	[ -d "${rootfs}" ] || die "${rootfs} is not a directory"

	init_path="/sbin/init"
	init="${rootfs}${init_path}"
	if [ ! -x "${init}" ] && [ ! -L "${init}" ]; then
		error "${init_path} is not installed in ${rootfs}"
		return 1
	fi
	OK "init is installed"


	candidate_systemd_paths="/usr/lib/systemd/systemd /lib/systemd/systemd"

	# check agent or systemd
	case "${AGENT_INIT}" in
		"no")
			for systemd_path in $candidate_systemd_paths; do
				systemd="${rootfs}${systemd_path}"
				if [ -x "${systemd}" ] || [ -L "${systemd}" ]; then
					found="yes"
					break
				fi
			done
			if [ ! $found ]; then
				error "None of ${candidate_systemd_paths} is installed in ${rootfs}"
				return 1
			fi
			OK "init is systemd"
			;;

		"yes")
			agent_path="/sbin/init"
			agent="${rootfs}${agent_path}"
			if  [ ! -x "${agent}" ]; then
				error "${agent_path} is not installed in ${rootfs}."
				return 1
			fi
			# checksum must be different to system
			for systemd_path in $candidate_systemd_paths; do
				systemd="${rootfs}${systemd_path}"
				if [ -f "${systemd}" ] && cmp -s "${systemd}" "${agent}"; then
					error "The agent is not the init process. ${agent_path} is systemd"
					return 1
				fi
			done

			OK "Agent installed"
			;;

		*)
			error "Invalid value for AGENT_INIT: '${AGENT_INIT}'. Use to 'yes' or 'no'"
			return 1
			;;
	esac

	return 0
}

calculate_required_disk_size() {
	local rootfs="$1"
	local fs_type="$2"
	local block_size="$3"

	rootfs_size_mb=$(du -B 1MB -s "${rootfs}" | awk '{print $1}')
	image="$(mktemp)"
	mount_dir="$(mktemp -d)"
	max_tries=20
	increment=10

	for i in $(seq 1 $max_tries); do
		local img_size="$((rootfs_size_mb + (i * increment)))"
		create_disk "${image}" "${img_size}" "${fs_type}" "${rootfs_start}" > /dev/null 2>&1
		OK "Create disk succeed in calculate_required_disk_size"
		if ! device="$(setup_loop_device "${image}")"; then
			continue
		fi

		if ! format_loop "${device}" "${block_size}" "${fs_type}" > /dev/null 2>&1 ; then
			die "Could not format loop device: ${device}"
		fi
		mount "${device}p1" "${mount_dir}"
		avail="$(df -BM --output=avail "${mount_dir}" | tail -n1 | sed 's/[M ]//g')"
		umount "${mount_dir}"
		losetup -d "${device}"
		rm -rf ${device} ${device}p1

		if [ "${avail}" -gt "${rootfs_size_mb}" ]; then
			rmdir "${mount_dir}"
			rm -f "${image}"
			echo "${img_size}"
			return
		fi
	done


	rmdir "${mount_dir}"
	rm -f "${image}"
	error "Could not calculate the required disk size"
}

# Calculate image size based on the rootfs and free space
calculate_img_size() {
	local rootfs="$1"
	local root_free_space_mb="$2"
	local fs_type="$3"
	local block_size="$4"

	# rootfs start + DAX header size + rootfs end
	local reserved_size_mb=$((rootfs_start + dax_header_sz + rootfs_end))

	disk_size="$(calculate_required_disk_size "${rootfs}" "${fs_type}" "${block_size}")"

	img_size="$((disk_size + reserved_size_mb))"
	if [ -n "${root_free_space_mb}" ]; then
		img_size="$((img_size + root_free_space_mb))"
	fi

	remaining="$((img_size % mem_boundary_mb))"
	if [ "${remaining}" != "0" ]; then
		img_size=$((img_size + mem_boundary_mb - remaining))
	fi

	echo "${img_size}"
}

setup_loop_device() {
	local image="$1"

	losetup -d /dev/loop236
	rm -rf /dev/loop236 /dev/loop236p1
	# just for get major of loop next step
	losetup -f > /dev/null
	# Get the loop device bound to the image file (requires /dev mounted in the
	# image build system and root privileges)
	# use /dev/loop236 as target loop device
	major=$(cat /proc/devices | grep loop | awk '{print$1}')
	device=/dev/loop236
	mknod $device b $major 236
	losetup -P $device ${image}

	#Refresh partition table
	partprobe -s "${device}" > /dev/null

	sleep 5

	mkdir /devtmp
	mount -t devtmpfs none /devtmp
	major_p=$(ls -l /devtmp/loop236p1 | awk '{print$5}' | awk -F ',' '{print$1}')
	minor_p=$(ls -l /devtmp/loop236p1 | awk '{print$6}')
	mknod /dev/loop236p1 b $major_p $minor_p

	umount /devtmp
	rmdir /devtmp
	echo "/dev/loop236"
}

format_loop() {
	local device="$1"
	local block_size="$2"
	local fs_type="$3"

	case "${fs_type}" in
		"${ext4_format}")
			mkfs.ext4 -q -F -b "${block_size}" "${device}p1"
			info "Set filesystem reserved blocks percentage to ${reserved_blocks_percentage}%"
			tune2fs -m "${reserved_blocks_percentage}" "${device}p1"
			;;

		"${xfs_format}")
			# DAX and reflink cannot be used together!
			# Explicitly disable reflink, if it fails then reflink
			# is not supported and '-m reflink=0' is not needed.
			if mkfs.xfs -m reflink=0 -q -f -b size="${block_size}" "${device}p1" 2>&1 | grep -q "unknown option"; then
				mkfs.xfs -q -f -b size="${block_size}" "${device}p1"
			fi
			;;

		*)
			error "Unsupported fs type: ${fs_type}"
			return 1
			;;
	esac
}

create_disk() {
	local image="$1"
	local img_size="$2"
	local fs_type="$3"
	local part_start="$4"

	info "Creating raw disk with size ${img_size}M"
	qemu-img create -q -f raw "${image}" "${img_size}M"
	OK "Image file created"

	# Kata runtime expect an image with just one partition
	# The partition is the rootfs content
	info "Creating partitions"
	parted -s -a optimal "${image}" -- \
		   mklabel gpt \
		   mkpart primary "${fs_type}" "${part_start}"M "${rootfs_end}"M

	OK "Partitions created"
}

create_rootfs_image() {
	local rootfs="$1"
	local image="$2"
	local img_size="$3"
	local fs_type="$4"
	local block_size="$5"

	create_disk "${image}" "${img_size}" "${fs_type}" "${rootfs_start}"
	OK "Create disk succeed"

	if ! device="$(setup_loop_device "${image}")"; then
		die "Could not setup loop device"
	fi

	if ! format_loop "${device}" "${block_size}" "${fs_type}"; then
		die "Could not format loop device: ${device}"
	fi

	info "Mounting root partition"
	mount_dir=$(mktemp -p ${TMPDIR:-/tmp} -d osbuilder-mount-dir.XXXX)
	mount "${device}p1" "${mount_dir}"
	OK "root partition mounted"

	info "Copying content from rootfs to root partition"
	cp -a "${rootfs}"/* "${mount_dir}"
	sync
	OK "rootfs copied"

	info "Removing unneeded systemd services and sockets"
	for u in "${systemd_units[@]}"; do
		find "${mount_dir}" -type f \( \
			 -name "${u}.service" -o \
			 -name "${u}.socket" \) \
			 -exec rm -f {} \;
	done

	info "Removing unneeded systemd files"
	for u in "${systemd_files[@]}"; do
		find "${mount_dir}" -type f -name "${u}" -exec rm -f {} \;
	done

	info "Creating empty machine-id to allow systemd to bind-mount it"
	touch "${mount_dir}/etc/machine-id"

	info "Unmounting root partition"
	umount "${mount_dir}"
	OK "Root partition unmounted"

	if [ "${fs_type}" = "${ext4_format}" ]; then
		fsck.ext4 -D -y "${device}p1"
	fi

	losetup -d "${device}"
	rm -f ${device} ${device}p1
	rmdir "${mount_dir}"
}

set_dax_header() {
	local image="$1"
	local img_size="$2"
	local fs_type="$3"
	local nsdax_bin="$4"

	# rootfs start + DAX header size
	local rootfs_offset=$((rootfs_start + dax_header_sz))
	local header_image="${image}.header"
	local dax_image="${image}.dax"
	rm -f "${dax_image}" "${header_image}"

	create_disk "${header_image}" "${img_size}" "${fs_type}" "${rootfs_offset}"

	dax_header_bytes=$((dax_header_sz * 1024 * 1024))
	dax_alignment_bytes=$((dax_alignment * 1024 * 1024))
	info "Set DAX metadata"
	# Set metadata header
	# Issue: https://github.com/kata-containers/osbuilder/issues/240
	if [ -z "${nsdax_bin}" ] ; then
		nsdax_bin="${script_dir}/nsdax"
		gcc -O2 "${script_dir}/nsdax.gpl.c" -o "${nsdax_bin}"
		trap "rm ${nsdax_bin}" EXIT
	fi
	"${nsdax_bin}" "${header_image}" "${dax_header_bytes}" "${dax_alignment_bytes}"
	sync

	touch "${dax_image}"
	# Copy MBR #1 + DAX metadata
	dd if="${header_image}" of="${dax_image}" bs="${dax_header_sz}M" count=1
	# Copy MBR #2 + Rootfs
	dd if="${image}" of="${dax_image}" oflag=append conv=notrunc
	# final image
	mv "${dax_image}" "${image}"
	sync

	rm -f "${dax_image}" "${header_image}"
}

install_prerequisites() {
	local os_distro="$1"
	if [ -z ${os_distro} ]; then
		. /etc/os-release
		os_distro=$ID
	fi
	case "${os_distro}" in
		ubuntu) apt-get update && apt-get install -y qemu-utils parted ;;
		centos) yum install -y qemu-img parted ;;
		euleros) yum install -y qemu-img parted ;;
		openEuler) yum install -y qemu-img parted ;;
		*) 
			error "${os_distro} is not supported" 
			exit 1
			;;
	esac
}

main() {
	[ "$(id -u)" -eq 0 ] || die "$0: must be run as root"
	
	# for creating and making partitions of image
	install_prerequisites
	
	# variables that can be overwritten by environment variables
	local agent_init="${AGENT_INIT:-yes}"
	local fs_type="${FS_TYPE:-${ext4_format}}"
	local image="${IMAGE:-/tmp/kuasar.img}"
	local block_size="${BLOCK_SIZE:-4096}"
	local root_free_space="${ROOT_FREE_SPACE:-}"
	local support_dax="${DAX:-yes}"
	local nsdax_bin="${NSDAX_BIN:-}"
	while getopts "ho:r:f:" opt
	do
		case "$opt" in
			h)	usage; return 0;;
			o)	image="${OPTARG}" ;;
			r)	root_free_space="${OPTARG}" ;;
			f)	fs_type="${OPTARG}" ;;
			*) break ;;
		esac
	done

	ROOTFS_DIR=${ROOTFS_DIR:-/tmp/kuasar-rootfs}
	rootfs="$(readlink -f "${ROOTFS_DIR}")"
	if [ -z "${rootfs}" ]; then
		usage
		exit 0
	fi

	if ! check_rootfs "${rootfs}" ; then
		die "Invalid rootfs"
	fi

	img_size=$(calculate_img_size "${rootfs}" "${root_free_space}" "${fs_type}" "${block_size}")

	# the first 2M are for the first MBR + NVDIMM metadata and were already
	# consider in calculate_img_size
	rootfs_img_size=$((img_size - dax_header_sz))
	create_rootfs_image "${rootfs}" "${image}" "${rootfs_img_size}" \
						"${fs_type}" "${block_size}"
        case "${support_dax}" in
	        "yes") 
                       # insert at the beginning of the image the MBR + DAX header
	               set_dax_header "${image}" "${img_size}" "${fs_type}" "${nsdax_bin}"
                       ;;
                *)     
                       echo "no need to set dax header"
                       ;;
	esac
}

main "$@"
