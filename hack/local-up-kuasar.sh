#!/usr/bin/env bash

# Copyright 2025 The Kuasar Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# This script builds and runs Kuasar services locally for testing.
# It supports selective compilation and startup of different sandbox implementations.
# Usage: hack/local-up-kuasar.sh [options]

set -o errexit
set -o nounset
set -o pipefail

# Setup repository root and library functions
KUASAR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "${KUASAR_ROOT}/hack/lib/init.sh"

# Default configuration
DEFAULT_COMPONENTS="runc,wasm"
LOG_DIR="${LOG_DIR:-${KUASAR_ROOT}/logs}"
PID_FILE="$LOG_DIR/kuasar.pid"

# Create log directory
mkdir -p "$LOG_DIR"

# Display usage help
show_help() {
    cat << EOF
Usage: $0 [OPTIONS]

Local Kuasar cluster startup script for testing.
Builds and runs Kuasar sandbox services following Kubernetes patterns.

Options:
  -c, --components COMPONENTS   Specify components to compile and start, comma-separated
                               Available values: runc,wasm,vmm,quark
                               Default: $DEFAULT_COMPONENTS
  -h, --help                   Show this help information
  --skip-build                 Skip compilation step, start existing binaries directly
  --debug                      Enable debug mode
  --check-deps                 Only check dependencies, do not compile and start

Description:
  This script automatically checks runtime dependencies and provides detailed 
  installation instructions if missing dependencies are found.

Examples:
  $0                           # Use default components (runc,wasm)
  $0 -c runc,                  # Only compile and start runc
  $0 -c runc --skip-build      # Only start pre-compiled runc component
  $0 --debug                   # Start in debug mode
  $0 --check-deps              # Only check dependency installation status

EOF
}

# Parse command line arguments
COMPONENTS="$DEFAULT_COMPONENTS"
SKIP_BUILD=false
DEBUG_MODE=false
CHECK_DEPS_ONLY=false

while [[ $# -gt 0 ]]; do
    case $1 in
        -c|--components)
            COMPONENTS="$2"
            shift 2
            ;;
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --debug)
            DEBUG_MODE=true
            shift
            ;;
        --check-deps)
            CHECK_DEPS_ONLY=true
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            kuasar::log::error "Unknown parameter: $1"
            show_help
            exit 1
            ;;
    esac
done

kuasar::log::info "Kuasar Local Startup Script"
kuasar::log::info "Working directory: $KUASAR_ROOT"
kuasar::log::info "Selected components: $COMPONENTS"

# Convert component string to array
IFS=',' read -ra COMPONENT_ARRAY <<< "$COMPONENTS"

# Cleanup function
cleanup() {
    kuasar::log::info "Cleaning up Kuasar processes..."
    
    # Terminate all Kuasar-related processes
    if [[ -f "$PID_FILE" ]]; then
        while IFS= read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                kuasar::log::info "Terminating process $pid"
                kill -TERM "$pid" 2>/dev/null || true
                sleep 1
                if kill -0 "$pid" 2>/dev/null; then
                    kuasar::log::warn "Force terminating process $pid"
                    kill -KILL "$pid" 2>/dev/null || true
                fi
            fi
        done < "$PID_FILE"
        rm -f "$PID_FILE"
    fi
    
    # Ensure all Kuasar-related processes are terminated using the PID file
    if [[ -f "$PID_FILE" ]]; then
        while IFS= read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                kuasar::log::info "Terminating process $pid"
                kill -TERM "$pid" 2>/dev/null || true
                sleep 1
                if kill -0 "$pid" 2>/dev/null; then
                    kuasar::log::warn "Force terminating process $pid"
                    kill -KILL "$pid" 2>/dev/null || true
                fi
            fi
        done < "$PID_FILE"
        rm -f "$PID_FILE"
    fi
    
    # Log a warning if any unexpected Kuasar-related processes are detected
    if pgrep -f "kuasar-" > /dev/null; then
        kuasar::log::warn "Detected Kuasar-related processes not tracked in PID file. Manual cleanup may be required."
    fi
    
    # Clean up temporary files
    kuasar::log::info "Cleaning temporary files..."
    rm -rf /tmp/kuasar-test-* 2>/dev/null || true
    rm -rf /var/run/kuasar* 2>/dev/null || true
    
    kuasar::log::info "Cleanup complete"
}

# Set automatic cleanup on exit
trap cleanup EXIT INT TERM

# Show installation instructions
show_installation_instructions() {
    local dep=$1
    
    case "$dep" in
        cargo|rustc)
            echo "    Install Rust:"
            echo "      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
            echo "      source ~/.cargo/env"
            ;;
        make)
            echo "    Install make:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y build-essential"
            echo "      # CentOS/RHEL:"
            echo "      sudo yum groupinstall -y 'Development Tools'"
            ;;
        gcc)
            echo "    Install gcc:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y gcc"
            echo "      # CentOS/RHEL:"
            echo "      sudo yum install -y gcc"
            ;;
        pkg-config)
            echo "    Install pkg-config:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y pkg-config"
            echo "      # CentOS/RHEL:"
            echo "      sudo yum install -y pkgconfig"
            ;;
        runc)
            echo "    Install runc:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y runc"
            echo "      # Or compile from source:"
            echo "      git clone https://github.com/opencontainers/runc.git"
            echo "      cd runc && make && sudo make install"
            ;;
        crictl)
            echo "    Install crictl:"
            echo "      # Download latest version:"
            echo "      VERSION=\"v1.28.0\""
            echo "      wget https://github.com/kubernetes-sigs/cri-tools/releases/download/\$VERSION/crictl-\$VERSION-linux-amd64.tar.gz"
            echo "      sudo tar zxvf crictl-\$VERSION-linux-amd64.tar.gz -C /usr/local/bin"
            echo "      rm -f crictl-\$VERSION-linux-amd64.tar.gz"
            ;;
        containerd)
            echo "    Install containerd:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y containerd"
            echo "      # Or from official binary:"
            echo "      wget https://github.com/containerd/containerd/releases/download/v1.7.0/containerd-1.7.0-linux-amd64.tar.gz"
            echo "      sudo tar Cxzvf /usr/local containerd-1.7.0-linux-amd64.tar.gz"
            ;;
        wasmedge)
            echo "    Install WasmEdge:"
            echo "      curl -sSf https://raw.githubusercontent.com/WasmEdge/WasmEdge/master/utils/install.sh | bash"
            echo "      source ~/.bashrc"
            ;;
        wasmtime)
            echo "    Install Wasmtime:"
            echo "      curl https://wasmtime.dev/install.sh -sSf | bash"
            echo "      source ~/.bashrc"
            ;;
        qemu-system-x86_64)
            echo "    Install QEMU:"
            echo "      # Ubuntu/Debian:"
            echo "      sudo apt-get update && sudo apt-get install -y qemu-system-x86"
            echo "      # CentOS/RHEL:"
            echo "      sudo yum install -y qemu-kvm"
            ;;
        *)
            echo "    Please refer to the official documentation for $dep installation instructions"
            ;;
    esac
    echo ""
}

# Check dependencies
check_dependencies() {
    kuasar::log::info "Checking runtime dependencies..."
    
    local missing_deps=()
    local missing_optional=()
    
    # Base dependencies
    local base_deps=("cargo" "rustc" "make" "gcc" "pkg-config")
    for dep in "${base_deps[@]}"; do
        if ! command -v "$dep" &> /dev/null; then
            missing_deps+=("$dep")
        fi
    done
    
    # Check Rust version
    if command -v rustc &> /dev/null; then
        local rust_version=$(rustc --version | awk '{print $2}')
        kuasar::log::info "Detected Rust version: $rust_version"
    fi
    
    # Check component-specific dependencies based on selected components
    for component in "${COMPONENT_ARRAY[@]}"; do
        case "$component" in
            runc)
                if ! command -v runc &> /dev/null; then
                    missing_deps+=("runc")
                fi
                ;;
            wasm)
                if ! command -v wasmedge &> /dev/null && ! command -v wasmtime &> /dev/null; then
                    missing_optional+=("wasmedge")
                    kuasar::log::warn "WasmEdge or Wasmtime not found, WASM functionality may not work properly"
                fi
                ;;
            vmm)
                if ! command -v qemu-system-x86_64 &> /dev/null; then
                    missing_optional+=("qemu-system-x86_64")
                    kuasar::log::warn "QEMU not found, VMM functionality may not work properly"
                fi
                ;;
        esac
    done
    
    # Check containerd related
    if ! command -v containerd &> /dev/null; then
        missing_optional+=("containerd")
        kuasar::log::warn "containerd not found, some functionality may be limited"
    fi
    
    if ! command -v crictl &> /dev/null; then
        missing_deps+=("crictl")
    fi
    
    # Report missing required dependencies
    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        kuasar::log::error "Missing the following required dependencies:"
        echo ""
        for dep in "${missing_deps[@]}"; do
            echo -e "${RED}  ✗ $dep${NC}"
            show_installation_instructions "$dep"
        done
        kuasar::log::error "Please install the missing required dependencies and try again"
        
        # If optional dependencies are missing, also show installation instructions
        if [[ ${#missing_optional[@]} -gt 0 ]]; then
            echo ""
            kuasar::log::warn "Missing the following optional dependencies (does not affect basic functionality):"
            echo ""
            for dep in "${missing_optional[@]}"; do
                echo -e "${YELLOW}  ! $dep${NC}"
                show_installation_instructions "$dep"
            done
        fi
        
        exit 1
    fi
    
    # Show installation instructions for optional dependencies
    if [[ ${#missing_optional[@]} -gt 0 ]]; then
        echo ""
        kuasar::log::warn "Recommend installing the following optional dependencies for full functionality:"
        echo ""
        for dep in "${missing_optional[@]}"; do
            echo -e "${YELLOW}  ! $dep${NC}"
            show_installation_instructions "$dep"
        done
        echo ""
        kuasar::log::warn "You can choose to continue, or install these dependencies first for better experience"
        read -p "Continue? (y/N): " -r
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            kuasar::log::info "Startup cancelled"
            exit 0
        fi
    fi
    
    kuasar::log::info "Dependency check complete ✓"
}

# Build component
build_component() {
    local component=$1
    kuasar::log::info "Building component: $component"
    
    case "$component" in
        runc)
            cd "$KUASAR_ROOT/runc"
            cargo build --release
            ;;
        wasm)
            cd "$KUASAR_ROOT/wasm"
            cargo build --release
            ;;
        vmm)
            cd "$KUASAR_ROOT/vmm"
            make build
            ;;
        quark)
            cd "$KUASAR_ROOT/quark"
            cargo build --release
            ;;
        *)
            kuasar::log::error "Unknown component: $component"
            return 1
            ;;
    esac
    
    kuasar::log::info "Component $component build complete ✓"
}

# Start component
start_component() {
    local component=$1
    kuasar::log::info "Starting component: $component"
    
    local binary=""
    local args=""
    local log_file="$LOG_DIR/$component.log"
    
    case "$component" in
        runc)
            binary="$KUASAR_ROOT/target/release/runc-sandboxer"
            args="--listen /run/kuasar-runc.sock --dir /run/kuasar-runc"
            ;;
        wasm)
            binary="$KUASAR_ROOT/target/release/wasm-sandboxer"
            args="--listen /run/kuasar-wasm.sock --dir /run/kuasar-wasm"
            ;;
        vmm)
            binary="$KUASAR_ROOT/target/release/vmm-sandboxer"
            args="--listen /run/kuasar-vmm.sock --dir /run/kuasar-vmm"
            ;;
        quark)
            binary="$KUASAR_ROOT/target/release/quark-sandboxer"
            args="--listen /run/kuasar-quark.sock --dir /run/kuasar-quark"
            ;;
        *)
            kuasar::log::error "Unknown component: $component"
            return 1
            ;;
    esac
    
    if [[ ! -f "$binary" ]]; then
        kuasar::log::error "Binary file does not exist: $binary"
        kuasar::log::error "Please build the component first or use --skip-build parameter"
        return 1
    fi
    
    # Create runtime directory
    local run_dir="/run/kuasar-$component"
    sudo mkdir -p "$run_dir"
    sudo chmod 777 "$run_dir"

    # Start component
    kuasar::log::info "Starting: $binary $args"
    if [[ "$DEBUG_MODE" == "true" ]]; then
        sudo "$binary" $args 2>&1 | tee "$log_file" &
    else
        sudo "$binary" $args > "$log_file" 2>&1 &
    fi
    
    local pid=$!
    echo "$pid" >> "$PID_FILE"
    
    # Wait for service to start
    sleep 2
    if kill -0 "$pid" 2>/dev/null; then
        kuasar::log::info "Component $component started successfully (PID: $pid) ✓"
    else
        kuasar::log::error "Component $component failed to start"
        kuasar::log::error "Check logs: $log_file"
        return 1
    fi
}

# Verify service status
verify_services() {
    kuasar::log::info "Verifying service status..."
    
    for component in "${COMPONENT_ARRAY[@]}"; do
        local sock_file="/run/kuasar-$component.sock"
        if [[ -S "$sock_file" ]]; then
            kuasar::log::info "Component $component socket OK: $sock_file ✓"
        else
            kuasar::log::warn "Component $component socket not found: $sock_file"
        fi
    done
}

# Show running status
show_status() {
    kuasar::log::info "Kuasar service status:"
    echo "===================="
    
    if [[ -f "$PID_FILE" ]]; then
        echo "Running processes:"
        while IFS= read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                local cmd=$(ps -p "$pid" -o comm= 2>/dev/null || echo "unknown")
                echo "  PID $pid: $cmd"
            fi
        done < "$PID_FILE"
    else
        echo "No running processes"
    fi
    
    echo ""
    echo "Socket files:"
    for component in "${COMPONENT_ARRAY[@]}"; do
        local sock_file="/run/kuasar-$component.sock"
        if [[ -S "$sock_file" ]]; then
            echo "  $component: $sock_file ✓"
        else
            echo "  $component: $sock_file ✗"
        fi
    done
    
    echo ""
    echo "Log file location: $LOG_DIR"
    echo "===================="
}

# Main function
main() {
    cd "$KUASAR_ROOT"
    
    # Check dependencies
    check_dependencies
    
    # If only checking dependencies, exit
    if [[ "$CHECK_DEPS_ONLY" == "true" ]]; then
        kuasar::log::info "Dependency check complete, exiting"
        exit 0
    fi
    
    # Build components
    if [[ "$SKIP_BUILD" == "false" ]]; then
        kuasar::log::info "Starting to build selected components..."
        for component in "${COMPONENT_ARRAY[@]}"; do
            build_component "$component"
        done
        kuasar::log::info "All components build complete ✓"
    else
        kuasar::log::info "Skipping build step"
    fi
    
    # Start components
    kuasar::log::info "Starting selected components..."
    for component in "${COMPONENT_ARRAY[@]}"; do
        start_component "$component"
    done
    
    # Verify services
    verify_services
    
    # Show status
    show_status
    
    kuasar::log::info "Kuasar services startup complete!"
    kuasar::log::info "You can now run test scripts in another terminal:"
    kuasar::log::info "  cd $KUASAR_ROOT/tests/basics"
    kuasar::log::info "  ./test-runc.sh"
    kuasar::log::info ""
    kuasar::log::info "Press Ctrl+C to stop all services"
    
    # Wait for user interruption
    while true; do
        sleep 1
    done
}

# Run main function
main "$@"
