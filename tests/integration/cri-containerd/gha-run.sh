#!/usr/bin/env bash

set -o errexit
set -o nounset
set -o pipefail

readonly script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
readonly repo_root="$(cd "${script_dir}/../../.." && pwd -P)"
readonly artifact_dir_default="${repo_root}/artifacts"

export HYPERVISOR="${HYPERVISOR:-clh}"
export ARTIFACTS_DIR="${ARTIFACTS_DIR:-${repo_root}/_artifacts/vmm-${HYPERVISOR}-cri-containerd}"
export KUASAR_ARTIFACTS_DIR="${KUASAR_ARTIFACTS_DIR:-${artifact_dir_default}}"
export CRICTL_VERSION="${CRICTL_VERSION:-v1.29.0}"
export CNI_PLUGINS_VERSION="${CNI_PLUGINS_VERSION:-v1.2.0}"
export CLOUD_HYPERVISOR_VERSION="${CLOUD_HYPERVISOR_VERSION:-v48.0}"

log() {
    printf '[cri-containerd] %s\n' "$*"
}

die() {
    printf '[cri-containerd] ERROR: %s\n' "$*" >&2
    exit 1
}

resolve_hypervisor_config() {
    case "${HYPERVISOR}" in
        clh)
            printf '%s\n' 'vmm/sandbox/config_clh.toml'
            ;;
        qemu)
            printf '%s\n' 'vmm/sandbox/config_qemu_x86_64.toml'
            ;;
        stratovirt)
            printf '%s\n' 'vmm/sandbox/config_stratovirt_x86_64.toml'
            ;;
        *)
            die "unsupported hypervisor: ${HYPERVISOR}"
            ;;
    esac
}

install_crictl() {
    local version="$1"
    local tarball="crictl-${version}-linux-amd64.tar.gz"
    curl -fsSL -o "/tmp/${tarball}" \
        "https://github.com/kubernetes-sigs/cri-tools/releases/download/${version}/${tarball}"
    sudo tar -C /usr/local/bin -xzf "/tmp/${tarball}"
    rm -f "/tmp/${tarball}"
}

install_cni_plugins() {
    local version="$1"
    local tarball="cni-plugins-linux-amd64-${version}.tgz"
    curl -fsSL -o "/tmp/${tarball}" \
        "https://github.com/containernetworking/plugins/releases/download/${version}/${tarball}"
    sudo mkdir -p /opt/cni/bin
    sudo tar -C /opt/cni/bin -xzf "/tmp/${tarball}"
    rm -f "/tmp/${tarball}"
}

install_cloud_hypervisor() {
    local version="$1"
    curl -fsSL -o /tmp/cloud-hypervisor-static \
        "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${version}/cloud-hypervisor-static"
    sudo install -m 0755 /tmp/cloud-hypervisor-static /usr/local/bin/cloud-hypervisor
    rm -f /tmp/cloud-hypervisor-static
}

ensure_virtiofsd() {
    if command -v virtiofsd >/dev/null 2>&1; then
        sudo ln -sf "$(command -v virtiofsd)" /usr/local/bin/virtiofsd
        return
    fi

    if [[ -x /usr/lib/qemu/virtiofsd ]]; then
        sudo ln -sf /usr/lib/qemu/virtiofsd /usr/local/bin/virtiofsd
        return
    fi

    if [[ -x /usr/libexec/virtiofsd ]]; then
        sudo ln -sf /usr/libexec/virtiofsd /usr/local/bin/virtiofsd
        return
    fi

    die "virtiofsd not found after dependency installation"
}

load_vhost_mods() {
    sudo modprobe vhost
    sudo modprobe vhost_net
    sudo modprobe vhost_vsock
}

install_dependencies() {
    local apt_packages=(
        curl
        jq
        iptables
        iproute2
        util-linux
        runc
        virtiofsd
    )

    log "Installing dependencies for ${HYPERVISOR}"

    case "${HYPERVISOR}" in
        qemu)
            apt_packages+=(
                qemu-system-x86
                qemu-utils
            )
            ;;
    esac

    sudo apt-get update
    sudo apt-get install -y --no-install-recommends "${apt_packages[@]}"

    install_crictl "${CRICTL_VERSION}"
    install_cni_plugins "${CNI_PLUGINS_VERSION}"
    install_cloud_hypervisor "${CLOUD_HYPERVISOR_VERSION}"
    ensure_virtiofsd
    load_vhost_mods
}

install_kuasar() {
    local artifacts_path="${1:-${KUASAR_ARTIFACTS_DIR}}"
    local config_relpath

    config_relpath="$(resolve_hypervisor_config)"

    log "Installing Kuasar artifacts from ${artifacts_path}"

    [[ -f "${artifacts_path}/bin/containerd" ]] || die "missing ${artifacts_path}/bin/containerd"
    [[ -f "${artifacts_path}/bin/config.toml" ]] || die "missing ${artifacts_path}/bin/config.toml"
    [[ -f "${artifacts_path}/bin/vmm-sandboxer" ]] || die "missing ${artifacts_path}/bin/vmm-sandboxer"
    [[ -f "${artifacts_path}/bin/vmlinux.bin" ]] || die "missing ${artifacts_path}/bin/vmlinux.bin"
    [[ -f "${artifacts_path}/bin/kuasar.img" ]] || die "missing ${artifacts_path}/bin/kuasar.img"
    [[ -f "${artifacts_path}/bin/kuasar-ctl" ]] || die "missing ${artifacts_path}/bin/kuasar-ctl"
    [[ -f "${artifacts_path}/${config_relpath}" ]] || die "missing ${artifacts_path}/${config_relpath}"

    sudo install -d -m 0755 /usr/local/bin /etc/containerd /var/lib/kuasar
    sudo install -m 0755 "${artifacts_path}/bin/containerd" /usr/local/bin/containerd
    sudo install -m 0755 "${artifacts_path}/bin/vmm-sandboxer" /usr/local/bin/vmm-sandboxer
    sudo install -m 0644 "${artifacts_path}/bin/config.toml" /etc/containerd/config.toml
    sudo install -m 0644 "${artifacts_path}/bin/vmlinux.bin" /var/lib/kuasar/vmlinux.bin
    sudo install -m 0644 "${artifacts_path}/bin/kuasar.img" /var/lib/kuasar/kuasar.img
    sudo install -m 0755 "${artifacts_path}/bin/kuasar-ctl" /usr/local/bin/kuasar-ctl
    sudo install -m 0644 "${artifacts_path}/${config_relpath}" /var/lib/kuasar/config.toml

    # Enable the guest debug console explicitly for kuasar-ctl.
    if sudo grep -q '^kernel_params = ""' /var/lib/kuasar/config.toml; then
        sudo sed -i 's/^kernel_params = ""$/kernel_params = "task.debug"/' \
            /var/lib/kuasar/config.toml
    elif ! sudo grep -q 'kernel_params = ".*task\.debug' /var/lib/kuasar/config.toml; then
        sudo sed -i 's/^kernel_params = "\(.*\)"$/kernel_params = "\1 task.debug"/' \
            /var/lib/kuasar/config.toml
    fi
}

run_tests() {
    log "Running cri-containerd tests using ${HYPERVISOR}"
    bash "${script_dir}/integration-tests.sh"
}

main() {
    local action="${1:-}"
    case "${action}" in
        install-dependencies) install_dependencies ;;
        install-kuasar) install_kuasar "${2:-${KUASAR_ARTIFACTS_DIR}}" ;;
        run) run_tests ;;
        *) die "invalid action: ${action}" ;;
    esac
}

main "$@"
