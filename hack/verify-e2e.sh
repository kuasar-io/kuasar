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

# This script verifies the e2e test environment setup and dependencies.
# Usage: hack/verify-e2e.sh

set -o errexit
set -o nounset
set -o pipefail

KUASAR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "${KUASAR_ROOT}/hack/lib/init.sh"

function check_rust_environment() {
    kuasar::log::info "Checking Rust environment..."
    
    if ! command -v cargo &> /dev/null; then
        kuasar::log::error "Cargo is not installed"
        kuasar::log::info "Please install Rust: https://rustup.rs/"
        return 1
    fi
    
    if ! command -v rustc &> /dev/null; then
        kuasar::log::error "Rustc is not installed"
        return 1
    fi
    
    local rust_version
    rust_version=$(rustc --version | cut -d' ' -f2)
    kuasar::log::info "Rust version: $rust_version"
    
    kuasar::log::success "Rust environment OK"
}

function check_cri_tools() {
    kuasar::log::info "Checking CRI tools..."
    
    if ! command -v crictl &> /dev/null; then
        kuasar::log::error "crictl is not installed"
        kuasar::log::info "Please install crictl: https://github.com/kubernetes-sigs/cri-tools"
        return 1
    fi
    
    local crictl_version
    crictl_version=$(crictl version --output json 2>/dev/null | grep -o '"version":"[^"]*' | cut -d'"' -f4 || echo "unknown")
    kuasar::log::info "crictl version: $crictl_version"
    
    kuasar::log::success "CRI tools OK"
}

function check_build_tools() {
    kuasar::log::info "Checking build tools..."
    
    local required_tools=("make" "gcc" "pkg-config")
    local missing_tools=()
    
    for tool in "${required_tools[@]}"; do
        if ! command -v "$tool" &> /dev/null; then
            missing_tools+=("$tool")
        fi
    done
    
    if [[ ${#missing_tools[@]} -gt 0 ]]; then
        kuasar::log::error "Missing build tools: ${missing_tools[*]}"
        kuasar::log::info "Install missing tools using your package manager"
        return 1
    fi
    
    kuasar::log::success "Build tools OK"
}

function check_test_framework() {
    kuasar::log::info "Checking test framework dependencies..."
    
    # Check if cargo is available (should be, since this is a Rust project)
    if ! command -v cargo &> /dev/null; then
        kuasar::log::error "Cargo is not installed (required for Rust-based e2e tests)"
        return 1
    fi
    
    local cargo_version
    cargo_version=$(cargo --version | cut -d' ' -f2)
    kuasar::log::info "Cargo version: $cargo_version"
    
    # Check e2e test directory structure
    local required_dirs=(
        "tests/e2e"
        "tests/e2e/src"
        "tests/e2e/configs"
        "hack"
        "hack/lib"
    )

    for dir in "${required_dirs[@]}"; do
        if [[ ! -d "${KUASAR_ROOT}/$dir" ]]; then
            kuasar::log::error "Missing required directory: $dir"
            return 1
        fi
    done

    # Check required files for Rust-based e2e tests
    local required_files=(
        "tests/e2e/Cargo.toml"
        "tests/e2e/src/lib.rs"
        "tests/e2e/src/main.rs"
        "tests/e2e/src/tests.rs"
        "hack/local-up-kuasar.sh"
        "hack/e2e-test.sh"
    )

    for file in "${required_files[@]}"; do
        if [[ ! -f "${KUASAR_ROOT}/$file" ]]; then
            kuasar::log::error "Missing required file: $file"
            return 1
        fi
    done
    
    # Check if e2e project builds
    kuasar::log::info "Verifying e2e project builds..."
    if ! (cd "${KUASAR_ROOT}/tests/e2e" && cargo check --quiet 2>/dev/null); then
        kuasar::log::error "E2E Rust project fails to build"
        return 1
    fi

    kuasar::log::success "Test framework OK"
}

function check_config_files() {
    kuasar::log::info "Checking test configuration files..."
    
    local config_dir="${KUASAR_ROOT}/tests/e2e/configs"
    local required_configs=(
        "sandbox-runc.yaml"
        "container-runc.yaml"
    )
    
    for config in "${required_configs[@]}"; do
        if [[ ! -f "${config_dir}/$config" ]]; then
            kuasar::log::error "Missing config file: tests/e2e/configs/$config"
            return 1
        fi
    done
    
    kuasar::log::success "Configuration files OK"
}

function check_permissions() {
    kuasar::log::info "Checking permissions..."
    
    # Check if we can write to common directories
    local test_dirs=("/tmp" "/var/run")
    
    for dir in "${test_dirs[@]}"; do
        if [[ -d "$dir" ]] && [[ ! -w "$dir" ]]; then
            kuasar::log::warn "No write permission to $dir - some tests may fail"
        fi
    done
    
    # Check if scripts are executable
    local scripts=(
        "hack/local-up-kuasar.sh"
        "hack/e2e-test.sh"
    )
    
    for script in "${scripts[@]}"; do
        if [[ ! -x "${KUASAR_ROOT}/$script" ]]; then
            kuasar::log::warn "Script not executable: $script"
            chmod +x "${KUASAR_ROOT}/$script"
            kuasar::log::info "Made script executable: $script"
        fi
    done
    
    kuasar::log::success "Permissions OK"
}

function check_system_requirements() {
    kuasar::log::info "Checking system requirements..."
    
    # Check operating system
    local os
    os=$(kuasar::util::detect_os)
    kuasar::log::info "Operating system: $os"
    
    if [[ "$os" != "linux" ]] && [[ "$os" != "macos" ]]; then
        kuasar::log::error "Unsupported operating system: $os"
        kuasar::log::info "Kuasar E2E tests require Linux or macOS"
        return 1
    fi
    
    # Check architecture
    local arch
    arch=$(kuasar::util::detect_arch)
    kuasar::log::info "Architecture: $arch"
    
    if [[ "$arch" != "x86_64" ]] && [[ "$arch" != "aarch64" ]]; then
        kuasar::log::warn "Architecture $arch may not be fully supported"
    fi
    
    kuasar::log::success "System requirements OK"
}

function run_verification() {
    kuasar::log::info "Starting Kuasar E2E environment verification..."
    kuasar::util::print_separator
    
    local checks=(
        "check_system_requirements"
        "check_rust_environment"
        "check_build_tools"
        "check_cri_tools"
        "check_test_framework"
        "check_config_files"
        "check_permissions"
    )
    
    local failed_checks=()
    
    for check in "${checks[@]}"; do
        if ! "$check"; then
            failed_checks+=("$check")
        fi
        echo
    done
    
    kuasar::util::print_separator
    
    if [[ ${#failed_checks[@]} -eq 0 ]]; then
        kuasar::log::success "All verification checks passed! ✓"
        kuasar::log::info "Your environment is ready for Kuasar E2E testing."
        kuasar::log::info ""
        kuasar::log::info "Next steps:"
        kuasar::log::info "  1. Run tests: make -f Makefile.e2e test-e2e"
        kuasar::log::info "     Or alternatively: make test-e2e"
        kuasar::log::info "  2. Or start local cluster: make -f Makefile.e2e local-up"
        kuasar::log::info "  3. See tests/e2e/README.md for detailed usage"
        return 0
    else
        kuasar::log::error "Verification failed with ${#failed_checks[@]} issues:"
        for check in "${failed_checks[@]}"; do
            kuasar::log::error "  - $check"
        done
        kuasar::log::info ""
        kuasar::log::info "Please resolve the issues above before running E2E tests."
        return 1
    fi
}

function show_help() {
    cat <<EOF
Usage: $0 [OPTIONS]

Verifies the Kuasar E2E testing environment setup and dependencies.

OPTIONS:
    -h, --help     Show this help message

EXAMPLES:
    # Run full verification
    hack/verify-e2e.sh
    
    # Show help
    hack/verify-e2e.sh --help

This script checks:
- Go and Rust development environments
- Build tools (make, gcc, pkg-config)
- CRI tools (crictl)
- Test framework dependencies
- Required files and directories
- System permissions
- Configuration files

EOF
}

function main() {
    case "${1:-}" in
        -h|--help)
            show_help
            exit 0
            ;;
        "")
            run_verification
            ;;
        *)
            kuasar::log::error "Unknown option: $1"
            show_help
            exit 1
            ;;
    esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
