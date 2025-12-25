# Kuasar VMM Installation Scripts

This directory contains installation and uninstallation scripts for Kuasar VMM (Virtual Machine Manager), enabling quick deployment and cleanup of Kuasar environments on different Linux systems.

## Scripts Overview

### 1. `build-and-install-kuasar-vmm.sh`
**Full-featured installation script that compiles all components from source**

- **Use Case**: Development environments, production environments requiring custom compilation
- **Supported Architectures**: x86_64, aarch64
- **Installation Method**: Source compilation with container-based builds

**Main Features**:
- Installs Cloud-Hypervisor VMM
- Compiles Kuasar VMM Sandboxer from source
- Compiles and installs Kuasar containerd version
- Installs and configures crictl
- Compiles virtiofsd from source
- Automatically detects and installs basic tools

### 2. `install-kuasar-vmm-x86.sh`
**Simplified x86 installation script using pre-compiled versions**

- **Use Case**: Quick deployment on x86_64 machines
- **Supported Architectures**: x86_64 only
- **Installation Method**: Pre-compiled binaries

**Main Features**:
- Installs Cloud-Hypervisor VMM
- Installs pre-compiled Kuasar version
- Installs and configures containerd
- Installs and configures crictl
- Installs pre-compiled virtiofsd

### 3. `uninstall-kuasar-vmm.sh`
**Uninstallation script to clean up Kuasar-related components**

- **Use Case**: Complete removal of Kuasar environment
- **Supported Architectures**: All
- **Uninstallation Method**: Interactive component selection

**Main Features**:
- Uninstalls Kuasar VMM Sandboxer
- Uninstalls Cloud-Hypervisor
- Optional uninstallation of containerd
- Optional uninstallation of crictl
- Uninstalls virtiofsd
- Cleans up configuration files and runtime files

## Usage

### Prerequisites

- **Operating System**: Linux (Ubuntu/Debian, RHEL/CentOS, Fedora, openSUSE, etc.)
- **Permissions**: sudo privileges required
- **Network**: Access to GitHub and related download sources
- **Container Runtime**: Docker or Podman (at least one)

### Basic Usage

```bash
# 1. Install from source compilation (recommended for development)
sudo ./build-and-install-kuasar-vmm.sh

# 2. Install using pre-compiled version (x86_64 only)
sudo ./install-kuasar-vmm-x86.sh

# 3. Uninstall Kuasar environment
sudo ./uninstall-kuasar-vmm.sh
```

### Installation Interactions

**Containerd Replacement Confirmation**:
```
WARNING: About to replace system containerd with kuasar version
   This will stop containerd service and replace the binary
   You can reinstall containerd anytime using your package manager
   Example: apt-get install --reinstall containerd.io

Do you want to continue? (y/N):
```

**Crictl Installation Confirmation**:
```
WARNING: About to install/replace crictl with Kuasar-compatible version
   This will replace existing crictl binary and configuration
   You can reinstall crictl anytime using your package manager
   Example: apt-get install --reinstall cri-tools

Do you want to continue? (y/N):
```

## Post-Installation Steps

After installation, you need to manually start the services with the correct parameters:

### Starting Containerd with ENABLE_CRI_SANDBOXES Parameter

**IMPORTANT**: containerd must be started with `ENABLE_CRI_SANDBOXES=1` environment variable to enable Kuasar VMM functionality.

```bash
# 1. Start containerd with ENABLE_CRI_SANDBOXES=1
sudo ENABLE_CRI_SANDBOXES=1 containerd &

# 2. Start vmm-sandboxer
sudo vmm-sandboxer &

# 3. Verify installation
sudo crictl version
sudo crictl info
```

**Note**: The scripts install containerd binary but do not create a systemd service. You need to start containerd manually with the required environment variable. For production use, you may want to create a custom systemd service file or use a process supervisor.

### Verify Kuasar VMM is Running

```bash
# Check if processes are running
ps aux | grep containerd
ps aux | grep vmm-sandboxer

# Test crictl connection
sudo crictl info
sudo crictl version

# Check runtime endpoints are available
ls -la /run/containerd/containerd.sock
```

## Using Kuasar VMM

After starting containerd and vmm-sandboxer, you can run the example script to test Kuasar VMM:

```bash
# Navigate to the examples directory
cd /path/to/kuasar/examples

# Run the example with Kuasar VMM
sudo ./run_example_container.sh kuasar-vmm
```

This script will:
- Pull a test container image
- Create and run a container using Kuasar VMM sandbox
- Verify the container is running correctly
- Clean up the test resources

For more information about using Kuasar VMM, refer to the main [README](../../README.md) and the [examples](../../examples/) directory.

## Uninstallation Interactions

The uninstall script will ask about removing specific components:

```bash
# Remove containerd?
WARNING: About to remove containerd binary and configuration installed by Kuasar
   This may affect container runtime functionality if you rely on it
   You can reinstall containerd anytime using your package manager

Do you want to remove containerd? (y/N):

# Remove crictl?
WARNING: About to remove crictl binary and configuration installed by Kuasar
   This may affect CRI tools functionality if you rely on it
   You can reinstall crictl anytime using your package manager

Do you want to remove crictl? (y/N):
```

## Dependencies

Scripts will automatically attempt to install the following basic tools:

- **git**: Source code cloning
- **wget**: File downloading
- **tar**: Archive extraction
- **make**: Build tool
- **unzip**: ZIP extraction (x86 script)

## Supported Package Managers

- **apt-get**: Ubuntu/Debian
- **yum**: RHEL/CentOS 7 and below
- **dnf**: RHEL/CentOS 8+, Fedora
- **zypper**: openSUSE

## Important Notes

1. **Backup Important Data**: Back up important container and configuration data before installation
2. **Service Interruption**: The installation process will stop containerd service, affecting running containers
3. **Network Connectivity**: Ensure access to GitHub and related download sites
4. **Disk Space**: Source compilation requires sufficient disk space (recommend 2GB+)
5. **System Compatibility**: Package managers may vary across different distributions

## Troubleshooting

### Common Issues

1. **Container Runtime Unavailable**:
   ```bash
   # Install Docker
   sudo apt-get install docker.io

   # Or install Podman
   sudo apt-get install podman
   ```

2. **Permission Issues**:
   ```bash
   # Ensure using sudo
   sudo ./build-and-install-kuasar-vmm.sh
   ```

3. **Network Issues**:
   ```bash
   # Check network connectivity
   curl -I https://github.com

   # Or configure proxy
   export https_proxy=http://your-proxy:port
   ```

4. **Compilation Failures**:
   - Check if disk space is sufficient
   - Ensure dependency tools are correctly installed
   - Review detailed error messages in build.log


```

## Version Management

Version information is defined at the top of each script:

```bash
CLOUD_HYPERVISOR_VERSION="v49.0"
KUASAR_VERSION="v1.0.1"
CRICTL_VERSION="v1.30.0"
VIRTIOFSD_VERSION="v1.13.2"
GOLANG_IMAGE_TAG="1.22-bullseye"
RUST_IMAGE_TAG="1.85-slim"
```

To upgrade versions, modify these variables to update all components used by the script.

---

**Disclaimer**: These scripts modify your system's container runtime environment. Please test thoroughly before using in production environments.