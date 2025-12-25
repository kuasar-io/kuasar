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
    exit $exit_code
}

# Trap for cleanup on script exit (normal or error)
trap cleanup EXIT INT TERM

# --- Version Definitions ---
# Core component versions - modify these to update entire script
CLOUD_HYPERVISOR_VERSION="v49.0"
KUASAR_VERSION="v1.0.1"
CRICTL_VERSION="v1.30.0"
VIRTIOFSD_VERSION="v1.13.2"

# --- Install basic tools (if not already installed) ---
install_basic_tools() {
    local tools="git wget tar unzip make"
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

# Change to temporary directory for all downloads and build operations
cd "${TEMP_DIR}"

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
# crictl checksums
CHECKSUMS["crictl-${CRICTL_VERSION}-linux-amd64.tar.gz"]="3dd03954565808eaeb3a7ffc0e8cb7886a64a9aa94b2bfdfbdc6e2ed94842e49"
# virtiofsd checksums
CHECKSUMS["virtiofsd-${VIRTIOFSD_VERSION}.zip"]="ff5304682a27975b5523ac3c9581c131ffab7d44bec6d2a0c001fec02bdb380c"

# --- 0. Check and Install Dependencies (Assuming a Debian/Ubuntu-like system) ---
# echo "--- 0. Installing necessary dependencies (git, wget, tar, unzip, make) ---"
# apt update
# apt install -y git wget tar unzip make

# --- 1. Install Cloud-Hypervisor VMM ---
echo "--- 1. Installing cloud-hypervisor ${CLOUD_HYPERVISOR_VERSION} ---"
CLH_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/${CLOUD_HYPERVISOR_VERSION}/cloud-hypervisor"

wget "${CLH_URL}"
verify_checksum "cloud-hypervisor" "${CHECKSUMS["cloud-hypervisor-${CLOUD_HYPERVISOR_VERSION}"]}"
chmod +x ./cloud-hypervisor
mv cloud-hypervisor /usr/local/bin/

# --- 2. Install Kuasar VMM Sandboxer and components ---
echo "--- 2. Installing Kuasar VMM Sandboxer and components ---"

git clone "https://github.com/kuasar-io/kuasar.git"
cd kuasar
git checkout "${KUASAR_VERSION}"
WORKDIR=$(pwd)

KUASAR_RELEASE="kuasar-${KUASAR_VERSION}-linux-amd64"
wget "https://github.com/kuasar-io/kuasar/releases/download/${KUASAR_VERSION}/${KUASAR_RELEASE}.tar.gz"
tar -zxvf "${KUASAR_RELEASE}.tar.gz"

# Move binaries and image files
echo "Moving Kuasar binaries..."
mkdir -p "${WORKDIR}/bin"

cp "${KUASAR_RELEASE}/vmm-sandboxer" "${WORKDIR}/bin/vmm-sandboxer"
cp "${KUASAR_RELEASE}/vmlinux.bin" "${WORKDIR}/bin/vmlinux.bin"
cp "${KUASAR_RELEASE}/kuasar.img" "${WORKDIR}/bin/kuasar.img"
cp "${KUASAR_RELEASE}/config_clh.toml" "${WORKDIR}/vmm/sandbox/config_clh.toml"

# Execute make install-vmm to copy necessary files to /usr/local/bin and /etc/kuasar
make install-vmm

# --- 3. Install and Configure Containerd (with backup) ---
echo "--- 3. Installing and configuring containerd ---"

# User confirmation for containerd replacement
echo "WARNING: About to replace system containerd with kuasar version"
echo "   This will replace the containerd binary and configuration"
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

# Stop containerd service before replacement
echo "Stopping containerd service..."
systemctl stop containerd 2>/dev/null || pkill containerd 2>/dev/null || true
sleep 2
echo "SUCCESS: containerd service stopped"

# Copy Kuasar's containerd binary
chmod +x "${KUASAR_RELEASE}/containerd"
cp "${KUASAR_RELEASE}/containerd" /usr/local/bin/
mkdir -p /etc/containerd
# Copy Kuasar-provided config (adjusted for Kuasar)
cp "${KUASAR_RELEASE}/config.toml" /etc/containerd/config.toml

# --- 4. Install and Configure crictl ---
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

CRICTL_URL="https://github.com/kubernetes-sigs/cri-tools/releases/download/${CRICTL_VERSION}/crictl-${CRICTL_VERSION}-linux-amd64.tar.gz"

wget "${CRICTL_URL}"
verify_checksum "crictl-${CRICTL_VERSION}-linux-amd64.tar.gz" "${CHECKSUMS["crictl-${CRICTL_VERSION}-linux-amd64.tar.gz"]}"
tar zxvf "crictl-${CRICTL_VERSION}-linux-amd64.tar.gz" -C /usr/local/bin

# Remove existing crictl config if exists
rm -f /etc/crictl.yaml

# Create crictl config file
echo "Creating /etc/crictl.yaml configuration file..."
tee /etc/crictl.yaml > /dev/null << EOF
runtime-endpoint: unix:///var/run/containerd/containerd.sock
image-endpoint: unix:///var/run/containerd/containerd.sock
timeout: 10
EOF

# --- 5. Install virtiofsd (for filesystem sharing) ---
echo "--- 5. Installing virtiofsd ---"
VIRTIOFSD_URL="https://gitlab.com/-/project/21523468/uploads/0298165d4cd2c73ca444a8c0f6a9ecc7/virtiofsd-${VIRTIOFSD_VERSION}.zip"
wget "${VIRTIOFSD_URL}"
verify_checksum "virtiofsd-${VIRTIOFSD_VERSION}.zip" "${CHECKSUMS["virtiofsd-${VIRTIOFSD_VERSION}.zip"]}"
unzip "virtiofsd-${VIRTIOFSD_VERSION}.zip"
mv target/x86_64-unknown-linux-musl/release/virtiofsd /usr/local/bin/

echo "--- SUCCESS: Kuasar + Cloud-Hypervisor Installation Completed Successfully! ---"
echo "Next step: Please manually start containerd and vmm-sandboxer, then run the example."