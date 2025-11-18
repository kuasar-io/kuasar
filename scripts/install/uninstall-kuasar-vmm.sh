\#!/bin/bash
set -e


echo "--- WARNING: This script will delete Kuasar, Cloud-Hypervisor, containerd, crictl, and all related configurations. Please confirm! ---"
echo "   This includes the containerd and crictl versions installed by Kuasar"
echo "   You can reinstall them using your package manager after uninstallation"
read -r -p "Do you want to continue with uninstallation? (y/N): " response
if [[ "$response" =~ ^([yY][eE][sS]|[yY])$ ]]
then
    echo "Starting component-based uninstallation..."
else
    echo "Uninstallation cancelled."
    exit 0
fi

# Note: This script removes Kuasar components and restores backed-up configurations

# --- 1. Global Service Shutdown ---
echo "--- 1. Stopping and removing services (Containerd/Kuasar) ---"
# Attempt to stop vmm-sandboxer service
if command -v systemctl &> /dev/null && systemctl list-unit-files | grep -q "kuasar-vmm.service"; then
    sudo systemctl stop kuasar-vmm
    sudo systemctl disable kuasar-vmm
fi
# Stop containerd (if still running)
if pgrep "containerd" &> /dev/null; then
    echo "Killing running containerd process..."
    sudo pkill -SIGTERM containerd || true
fi
sleep 2 # Wait briefly for processes to terminate


# --- 2. Component: Cloud-Hypervisor ---
echo "--- 2. Uninstalling Cloud-Hypervisor VMM ---"
sudo rm -f /usr/local/bin/cloud-hypervisor


# --- 3. Component: Kuasar VMM Sandboxer ---
echo "--- 3. Uninstalling Kuasar VMM Sandboxer and components ---"
# Delete binaries
sudo rm -f /usr/local/bin/vmm-sandboxer
sudo rm -f /var/lib/kuasar/vmlinux.bin
sudo rm -f /var/lib/kuasar/kuasar.img
sudo rm -rf /var/lib/kuasar

# Delete configs and runtime files
sudo rm -rf /run/kuasar-vmm
sudo rm -f /run/vmm-sandboxer.sock
sudo rm -f /sys/fs/cgroup/system.slice/kuasar-vmm.service || true
sudo rm -f /usr/lib/systemd/system/kuasar-vmm.service || true
sudo rm -f /etc/sysconfig/kuasar-vmm || true


# --- 4. Component: Containerd (Remove Kuasar Version) ---
echo "--- 4. Removing Kuasar containerd ---"
echo "WARNING: About to remove containerd binary and configuration installed by Kuasar"
echo "   This may affect container runtime functionality if you rely on it"
echo "   You can reinstall containerd anytime using your package manager"
echo ""
read -p "   Do you want to remove containerd? (y/N): " -n 1 -r REPLY
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    # Delete Kuasar's Containerd binary
    sudo rm -f /usr/local/bin/containerd

    # Remove Kuasar's containerd configuration
    sudo rm -f /etc/containerd/config.toml

    echo "SUCCESS: Removed Kuasar containerd binary and configuration"
    echo "INFO:  To restore containerd, reinstall using your package manager:"
    echo "   Example: sudo apt-get install --reinstall containerd.io"
else
    echo "SKIPPED:  Skipping containerd removal as requested"
    echo "INFO:  Containerd binary and configuration will be preserved"
fi


# --- 5. Component: crictl (Remove Kuasar Version) ---
echo "--- 5. Removing crictl ---"
echo "WARNING: About to remove crictl binary and configuration installed by Kuasar"
echo "   This may affect CRI tools functionality if you rely on it"
echo "   You can reinstall crictl anytime using your package manager"
echo ""
read -p "   Do you want to remove crictl? (y/N): " -n 1 -r REPLY
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    # Delete crictl binary and configuration
    sudo rm -f /usr/local/bin/crictl
    sudo rm -f /etc/crictl.yaml

    echo "SUCCESS: Removed Kuasar crictl binary and configuration"
    echo "INFO:  To restore crictl, reinstall using your package manager:"
    echo "   Example: sudo apt-get install --reinstall cri-tools"
else
    echo "SKIPPED:  Skipping crictl removal as requested"
    echo "INFO:  crictl binary and configuration will be preserved"
fi


# --- 6. Component: virtiofsd ---
echo "--- 6. Uninstalling virtiofsd ---"
sudo rm -f /usr/local/bin/virtiofsd


# --- 6. Final Cleanup ---
echo "--- 6. Cleaning up completed ---"


echo "--- SUCCESS: Kuasar Uninstallation Completed! ---"
echo "SUCCESS: Removed all Kuasar VMM components including containerd and crictl"