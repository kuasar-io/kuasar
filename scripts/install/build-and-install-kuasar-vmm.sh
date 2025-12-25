#!/bin/bash
set -e


# --- Create Temporary Directory and Setup Cleanup Trap ---
TEMP_DIR=$(mktemp -d)
echo "Created temporary directory: ${TEMP_DIR}"

# Cleanup function to remove temporary files
cleanup() {
    local exit_code=$?
    echo "Cleaning up temporary directory: ${TEMP_DIR}"
    # Change back to original directory before cleanup
    cd - > /dev/null 2>&1 || true
    rm -rf "${TEMP_DIR}"
    rm -rf /tmp/kuasar-rootfs/
    rm -rf /tmp/linux-cloud-hypervisor/
    exit $exit_code
}

# Trap for cleanup on script exit (normal or error)
trap cleanup EXIT INT TERM

MACHINE_ARCH=$(uname -m)
PACKAGE_ARCH="$(dpkg --print-architecture)"

# RUNTIME will be set after container runtime detection
# This ensures build.sh uses the same container runtime we detect and validate

# --- Version Definitions ---
# Core component versions - modify these to update the entire script
CLOUD_HYPERVISOR_VERSION="v49.0"
KUASAR_VERSION="v1.0.1"
CRICTL_VERSION="v1.30.0"
VIRTIOFSD_VERSION="v1.13.2"
GOLANG_IMAGE_TAG="1.22-bullseye"
RUST_IMAGE_TAG="1.85-slim"

# Usage: Modify the VERSION variables above to upgrade all components throughout the script
# This makes version management centralized and easier to maintain

# --- Checksum Validation Function ---
verify_checksum() {
    local file="$1"
    local expected="$2"
    local actual=$(sha256sum "$file" | cut -d' ' -f1)

    if [[ "$actual" != "$expected" ]]; then
        echo "ERROR: Checksum verification failed for $file"
        echo "Expected: $expected"
        echo "Actual: $actual"
        echo "This may indicate a corrupted download or security issue!"
        exit 1
    fi
    echo "SUCCESS: Checksum verified for $file"
}

# --- Expected Checksums ---
declare -A CHECKSUMS
# Cloud-Hypervisor checksums
CHECKSUMS["cloud-hypervisor-${CLOUD_HYPERVISOR_VERSION}"]="a96626dce1c2cfc78feb257bea893e549566595a274a11266512ad6097f959aa"
CHECKSUMS["cloud-hypervisor-static-aarch64-${CLOUD_HYPERVISOR_VERSION}"]="sha256:PLACEHOLDER_CLOUD_HYPERVISOR_ARM_CHECKSUM"  # Need ARM64 checksum
# crictl checksums
CHECKSUMS["crictl-${CRICTL_VERSION}-linux-amd64.tar.gz"]="3dd03954565808eaeb3a7ffc0e8cb7886a64a9aa94b2bfdfbdc6e2ed94842e49"
CHECKSUMS["crictl-${CRICTL_VERSION}-linux-arm64.tar.gz"]="sha256:PLACEHOLDER_CRICTL_ARM64_CHECKSUM"  # Need ARM64 checksum
# virtiofsd source (we build from source, but can verify the git tag)
# For git repos, we typically verify tags rather than checksums of the source

# --- 0. Check and Install Dependencies (Assuming a Debian/Ubuntu-like system) ---
# echo "--- 0. Installing necessary dependencies (git, wget, tar, unzip, make) ---"
# sudo apt update
# sudo apt install -y git wget tar unzip make

# Check for container runtime (ctr, docker, or podman) and validate it's working
# --- Install basic tools (if not already installed) ---
install_basic_tools() {
    local tools="git wget tar make"
    echo "--- Installing basic tools ---"

    if command -v apt-get >/dev/null 2>&1; then
        echo "Using apt-get package manager"
        echo "Installing: $tools"
        sudo apt-get update -qq
        sudo apt-get install $tools
    elif command -v yum >/dev/null 2>&1; then
        echo "Using yum package manager"
        echo "Installing: $tools"
        sudo yum install $tools
    elif command -v dnf >/dev/null 2>&1; then
        echo "Using dnf package manager"
        echo "Installing: $tools"
        sudo dnf install $tools
    elif command -v zypper >/dev/null 2>&1; then
        echo "Using zypper package manager"
        echo "Installing: $tools"
        sudo zypper install $tools
    else
        echo "No supported package manager found. Please ensure the following tools are installed:"
        echo "  $tools"
        return 1
    fi
}

install_basic_tools

# Set RUNTIME for build.sh compatibility (build.sh defaults to "containerd" if not set)
echo "--- Checking for container runtime ---"
if command -v ctr &> /dev/null; then
    # Check if containerd is accessible via ctr
    if ctr version &> /dev/null; then
        CONTAINER_RUNTIME="ctr"
        echo "SUCCESS: Found and validated container runtime: $CONTAINER_RUNTIME"
    else
        echo "ERROR: ctr found but containerd is not accessible"
        echo "   Please ensure containerd service is running: sudo systemctl start containerd"
        exit 1
    fi
elif command -v docker &> /dev/null; then
    # Check if docker daemon is accessible
    if docker version &> /dev/null; then
        CONTAINER_RUNTIME="docker"
        echo "SUCCESS: Found and validated container runtime: $CONTAINER_RUNTIME"
    else
        echo "ERROR: docker found but daemon is not accessible"
        echo "   Please ensure docker service is running: sudo systemctl start docker"
        exit 1
    fi
elif command -v podman &> /dev/null; then
    # Check if podman is accessible
    if podman version &> /dev/null; then
        CONTAINER_RUNTIME="podman"
        echo "SUCCESS: Found and validated container runtime: $CONTAINER_RUNTIME"
    else
        echo "ERROR: podman found but not accessible"
        echo "   Please check podman installation and permissions"
        exit 1
    fi
else
    echo "ERROR: No container runtime found (ctr/docker/podman). Please install one of them first."
    echo "   Recommended: sudo apt-get install containerd.io"
    exit 1
fi

# Set RUNTIME for build.sh compatibility (build.sh expects RUNTIME variable)
export RUNTIME="${CONTAINER_RUNTIME}"
echo "SUCCESS: Exported RUNTIME=${RUNTIME} for build.sh compatibility"

# --- 1. Install Cloud-Hypervisor VMM ---
cd "${TEMP_DIR}"
echo "--- 1. Installing cloud-hypervisor ${CLOUD_HYPERVISOR_VERSION} ---"
CLH_VERSION="${CLOUD_HYPERVISOR_VERSION}"
CLH_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CLH_VERSION}/cloud-hypervisor"
CLH_ARM_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CLH_VERSION}/cloud-hypervisor-static-aarch64"

if [ "$MACHINE_ARCH" = "x86_64" ]; then
    wget -O cloud-hypervisor "${CLH_URL}"
    verify_checksum "cloud-hypervisor" "${CHECKSUMS["cloud-hypervisor-${CLOUD_HYPERVISOR_VERSION}"]}"
elif [ "$MACHINE_ARCH" = "aarch64" ]; then
    wget -O cloud-hypervisor "${CLH_ARM_URL}"
    verify_checksum "cloud-hypervisor" "${CHECKSUMS["cloud-hypervisor-static-aarch64-${CLOUD_HYPERVISOR_VERSION}"]}"
else
    echo "Unsupported architecture: $MACHINE_ARCH"
    exit 1
fi

chmod +x ./cloud-hypervisor
# Set cap_net_admin capability to allow network operations
sudo setcap cap_net_admin+ep ./cloud-hypervisor
sudo mv cloud-hypervisor /usr/local/bin/
echo "SUCCESS: cloud-hypervisor installed successfully"

# --- 2. Download Kuasar Source Code and Build VMM Sandboxer ---
cd "${TEMP_DIR}"
echo "--- 2. Downloading and building Kuasar VMM Sandboxer ---"

# Download kuasar source code
git clone "https://github.com/kuasar-io/kuasar.git"
cd kuasar
git checkout "${KUASAR_VERSION}"
KUASAR_WORKDIR=$(pwd)
echo "SUCCESS: Kuasar source code ready at ${KUASAR_WORKDIR}"

echo "Building Kuasar VMM Sandboxer using $CONTAINER_RUNTIME..."
# Build kuasar in Rust container with mounted source
$CONTAINER_RUNTIME run --rm \
    -v "${TEMP_DIR}":/workspace \
    -w /workspace/kuasar \
    rust:${RUST_IMAGE_TAG} \
    bash -c "
        apt-get update && \
        apt-get install -y cmake build-essential && \
        export HYPERVISOR=cloud_hypervisor && \
        make bin/vmm-sandboxer
    "
cd "${KUASAR_WORKDIR}"
make bin/kuasar.img 
make bin/vmlinux.bin
make install-vmm

# echo "SUCCESS: Kuasar VMM Sandboxer built and installed successfully"

# --- 3. Replace containerd binary with kuasar version ---
cd "${TEMP_DIR}"
echo "--- 3. Replacing containerd binary with kuasar version ---"

# User confirmation for containerd replacement
echo "WARNING: About to replace system containerd with kuasar version"
echo "   This will stop containerd service and replace the binary"
echo "   You can reinstall containerd anytime using your package manager"
echo "   Example: apt-get install --reinstall containerd.io"
echo ""
read -p "   Do you want to continue? (y/N): " -n 1 -r REPLY
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "ERROR: containerd replacement cancelled by user"
    exit 1
fi
echo "SUCCESS: User confirmed containerd replacement"

# Build containerd using kuasar source in container
echo "Building containerd using $CONTAINER_RUNTIME..."

sudo rm -rf "${KUASAR_WORKDIR}/bin"
# Build containerd in container
$CONTAINER_RUNTIME run --rm \
    -v "${TEMP_DIR}":/workspace \
    -w /workspace/kuasar \
    golang:${GOLANG_IMAGE_TAG} \
    bash -c "
        apt-get update && \
        apt-get install -y make git && \
        bash scripts/build/build-containerd.sh && \
        chmod +x bin/containerd
    "
# Simple binary replacement without service modification
echo "Stopping containerd service for binary replacement..."
sudo systemctl stop containerd 2>/dev/null || echo "containerd service was not running"

# Replace the binary only
sudo cp "${KUASAR_WORKDIR}/bin/containerd" /usr/local/bin/
echo "SUCCESS: containerd binary replaced with kuasar version"

# Ensure config directory exists and copy config
sudo mkdir -p /etc/containerd
sudo cp "${KUASAR_WORKDIR}/bin/config.toml" /etc/containerd/config.toml
echo "SUCCESS: containerd config updated"

# --- 4. Install and Configure crictl ---
cd "${TEMP_DIR}"
echo "--- 4. Installing and configuring crictl ---"

# User confirmation for crictl replacement
echo "WARNING: About to install/replace crictl with Kuasar-compatible version"
echo "   This will replace existing crictl binary and configuration"
echo "   You can reinstall crictl anytime using your package manager"
echo "   Example: apt-get install --reinstall cri-tools"
echo ""
read -p "   Do you want to continue? (y/N): " -n 1 -r REPLY
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "ERROR: crictl installation cancelled by user"
    exit 1
fi
echo "SUCCESS: User confirmed crictl installation"

CRICTL_URL="https://github.com/kubernetes-sigs/cri-tools/releases/download/${CRICTL_VERSION}/crictl-${CRICTL_VERSION}-linux-${PACKAGE_ARCH}.tar.gz"

wget "${CRICTL_URL}"
verify_checksum "crictl-${CRICTL_VERSION}-linux-${PACKAGE_ARCH}.tar.gz" "${CHECKSUMS["crictl-${CRICTL_VERSION}-linux-${PACKAGE_ARCH}.tar.gz"]}"
sudo tar zxvf "crictl-${CRICTL_VERSION}-linux-${PACKAGE_ARCH}.tar.gz" -C /usr/local/bin
# Cleanup is handled by trap, no need for explicit rm

# Remove existing crictl config if exists
sudo rm -f /etc/crictl.yaml

# Create crictl config file
echo "Creating /etc/crictl.yaml configuration file..."
sudo tee /etc/crictl.yaml > /dev/null << EOF
runtime-endpoint: unix:///var/run/containerd/containerd.sock
image-endpoint: unix:///var/run/containerd/containerd.sock
timeout: 10
EOF

# --- 5. Build and Install virtiofsd from source in container ---
cd "${TEMP_DIR}"
echo "--- 5. Building virtiofsd from source for $MACHINE_ARCH ---"

# Determine Rust target for static compilation
RUST_TARGET=""
if [ "$MACHINE_ARCH" = "x86_64" ]; then
    RUST_TARGET="x86_64-unknown-linux-musl"
elif [ "$MACHINE_ARCH" = "aarch64" ]; then
    RUST_TARGET="aarch64-unknown-linux-musl"
else
    echo "ERROR: Unsupported architecture for static compilation: $MACHINE_ARCH"
    exit 1
fi
echo "SUCCESS: Rust target for static compilation: $RUST_TARGET"

# Build virtiofsd in current temp directory
echo "Cloning virtiofsd repository..."
git clone https://gitlab.com/virtio-fs/virtiofsd.git
cd virtiofsd
git checkout ${VIRTIOFSD_VERSION}

echo "Building virtiofsd for target: $RUST_TARGET"

# Build virtiofsd in Rust container
echo "Building virtiofsd using $CONTAINER_RUNTIME..."
$CONTAINER_RUNTIME run --rm \
    -v "${TEMP_DIR}":/workspace \
    -w /workspace/virtiofsd \
    rust:${RUST_IMAGE_TAG} \
    bash -c "
        apt-get update && \
        apt-get install -y musl-tools libcap-ng-dev libseccomp-dev && \
        rustup target add ${RUST_TARGET} && \
        cargo build --release --target ${RUST_TARGET}
    "

# Copy the built binary
echo "Installing virtiofsd binary..."
sudo cp target/${RUST_TARGET}/release/virtiofsd /usr/local/bin/
sudo chmod +x /usr/local/bin/virtiofsd

echo "SUCCESS: virtiofsd built and installed successfully"

echo "--- SUCCESS: Kuasar + Cloud-Hypervisor Installation Completed Successfully! ---"
echo "Next step: Please manually start containerd with arg ENABLE_CRI_SANDBOXES=1, then start kuasar-vmm. "