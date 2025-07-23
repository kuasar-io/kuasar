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

# This script runs Kuasar e2e tests using Rust.
# Usage: hack/e2e-test.sh [options]

set -o errexit
set -o nounset
set -o pipefail

KUASAR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "${KUASAR_ROOT}/hack/lib/init.sh"

# Test configuration
RUNTIME="${RUNTIME:-"runc,wasm"}"
ARTIFACTS="${ARTIFACTS:-"${KUASAR_ROOT}/_artifacts/$(date +%Y%m%d-%H%M%S)"}"
PARALLEL="${PARALLEL:-false}"
LOG_LEVEL="${LOG_LEVEL:-"info"}"
TEST_TIMEOUT="${TEST_TIMEOUT:-"300"}"

function usage() {
    cat <<EOF
Usage: $0 [options]

Runs Kuasar E2E tests using Rust testing framework.

Options:
    -h, --help              Show this help message
    --runtime RUNTIMES      Comma-separated list of runtimes to test (default: runc,wasm)
    --artifacts DIR         Directory to store test artifacts
    --parallel              Run tests in parallel
    --log-level LEVEL       Log level for tests (default: info)
    --timeout SECONDS       Test timeout in seconds (default: 300)

Environment Variables:
    ARTIFACTS               Test artifacts directory
    RUNTIME                 Runtimes to test
    PARALLEL               Enable parallel execution
    LOG_LEVEL              Logging level
    RUST_LOG               Rust logging configuration

Examples:
    # Run all tests
    $0

    # Test only runc runtime
    $0 --runtime runc

    # Run tests in parallel with debug logging
    $0 --parallel --log-level debug

    # Test with custom artifacts directory
    $0 --artifacts /tmp/my-test-artifacts
EOF
}

function parse_args() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            -h|--help)
                usage
                exit 0
                ;;
            --runtime)
                RUNTIME="$2"
                shift 2
                ;;
            --artifacts)
                ARTIFACTS="$2"
                shift 2
                ;;
            --parallel)
                PARALLEL="true"
                shift
                ;;
            --log-level)
                LOG_LEVEL="$2"
                shift 2
                ;;
            --timeout)
                TEST_TIMEOUT="$2"
                shift 2
                ;;
            *)
                echo "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done
}

function setup_directories() {
    echo "Setting up test environment..."
    
    # Create artifacts directory
    mkdir -p "${ARTIFACTS}"
    echo "Artifacts directory: ${ARTIFACTS}"
    
    # Set environment variables
    export ARTIFACTS="${ARTIFACTS}"
    export KUASAR_LOG_LEVEL="${LOG_LEVEL}"
    export RUST_LOG="${RUST_LOG:-${LOG_LEVEL}}"
    
    # Ensure we're in the correct directory
    cd "${KUASAR_ROOT}"
}

function check_dependencies() {
    echo "Checking test dependencies..."
    
    # Check if crictl is available
    if ! command -v crictl &> /dev/null; then
        echo "ERROR: crictl is required for e2e tests"
        echo "Please install crictl: https://github.com/kubernetes-sigs/cri-tools"
        exit 1
    fi
    
    # Check if required build tools are available
    local required_tools=("cargo" "rustc" "make")
    for tool in "${required_tools[@]}"; do
        if ! command -v "$tool" &> /dev/null; then
            echo "ERROR: $tool is required for building Kuasar components"
            exit 1
        fi
    done
    
    echo "Dependencies check passed."
}

function build_test_binaries() {
    echo "Building e2e test binary..."
    
    cd "${KUASAR_ROOT}/tests/e2e"
    cargo build --release --bin kuasar-e2e
    
    if [[ ! -f "target/release/kuasar-e2e" ]]; then
        echo "ERROR: Failed to build e2e test binary"
        exit 1
    fi
    
    echo "E2E test binary built successfully"
}

function run_e2e_tests() {
    echo "Running Kuasar E2E tests..."
    echo "Runtime(s): ${RUNTIME}"
    echo "Parallel: ${PARALLEL}"
    echo "Log Level: ${LOG_LEVEL}"
    echo "Timeout: ${TEST_TIMEOUT}s"
    
    cd "${KUASAR_ROOT}/tests/e2e"
    
    local test_args=()
    test_args+=(--runtime "${RUNTIME}")
    
    if [[ "${PARALLEL}" == "true" ]]; then
        test_args+=(--parallel)
    fi
    
    # Run the e2e binary
    echo "Executing: ./target/release/kuasar-e2e ${test_args[*]}"
    timeout "${TEST_TIMEOUT}" ./target/release/kuasar-e2e "${test_args[@]}" || {
        local exit_code=$?
        echo "E2E tests failed with exit code: ${exit_code}"
        return ${exit_code}
    }
    
    echo "E2E tests completed successfully"
}

function run_rust_unit_tests() {
    echo "Running Rust unit tests..."
    
    cd "${KUASAR_ROOT}/tests/e2e"
    
    # Run unit tests with proper logging
    RUST_LOG="${LOG_LEVEL}" cargo test --release -- --test-threads=1 --nocapture || {
        local exit_code=$?
        echo "Rust unit tests failed with exit code: ${exit_code}"
        return ${exit_code}
    }
    
    echo "Rust unit tests completed successfully"
}

function cleanup() {
    echo "Cleaning up test environment..."
    
    # Kill any remaining processes
    pkill -f "runc-sandboxer" || true
    pkill -f "wasm-sandboxer" || true
    
    # Clean up containers and sandboxes
    crictl rm --all --force 2>/dev/null || true
    crictl rmp --all --force 2>/dev/null || true
    
    echo "Cleanup completed"
}

function main() {
    parse_args "$@"
    
    # Set up signal handlers
    trap cleanup EXIT
    trap 'cleanup; exit 130' INT
    trap 'cleanup; exit 143' TERM
    
    echo "Starting Kuasar E2E tests..."
    echo "Kuasar root: ${KUASAR_ROOT}"
    echo "Runtime(s): ${RUNTIME}"
    echo "Parallel: ${PARALLEL}"
    echo "Timeout: ${TEST_TIMEOUT}s"
    echo "Log level: ${LOG_LEVEL}"
    
    setup_directories
    check_dependencies  
    build_test_binaries
    
    # Run the standalone e2e binary
    run_e2e_tests
    
    echo "All tests completed successfully!"
    echo "Test results available in: ${ARTIFACTS}"
}

# Run main function with all arguments
main "$@"
