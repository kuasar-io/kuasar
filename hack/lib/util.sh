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

# Utility functions for Kuasar hack scripts

# kuasar::util::ensure_clean_working_dir ensures that the working directory is clean
function kuasar::util::ensure_clean_working_dir() {
    if ! git diff --quiet HEAD; then
        kuasar::log::error "Working directory is not clean. Please commit or stash changes."
        exit 1
    fi
}

# kuasar::util::find_binary finds a binary in the standard locations
function kuasar::util::find_binary() {
    local binary="$1"
    local binary_path
    
    # Try common build output locations
    for dir in "${KUASAR_ROOT}/target/release" "${KUASAR_ROOT}/target/debug" "/usr/local/bin" "/usr/bin"; do
        if [[ -x "${dir}/${binary}" ]]; then
            binary_path="${dir}/${binary}"
            break
        fi
    done
    
    if [[ -z "${binary_path:-}" ]]; then
        # Try PATH
        if command -v "${binary}" &> /dev/null; then
            binary_path="$(command -v "${binary}")"
        fi
    fi
    
    echo "${binary_path:-}"
}

# kuasar::util::wait_for_condition waits for a condition to become true
function kuasar::util::wait_for_condition() {
    local description="$1"
    local timeout="$2"
    local interval="${3:-1}"
    shift 3
    
    local elapsed=0
    while [[ ${elapsed} -lt ${timeout} ]]; do
        if "$@"; then
            kuasar::log::info "${description} succeeded after ${elapsed}s"
            return 0
        fi
        
        sleep "${interval}"
        elapsed=$((elapsed + interval))
    done
    
    kuasar::log::error "${description} timed out after ${timeout}s"
    return 1
}

# kuasar::util::check_required_tools checks if required tools are installed
function kuasar::util::check_required_tools() {
    local tools=("$@")
    local missing_tools=()
    
    for tool in "${tools[@]}"; do
        if ! command -v "${tool}" &> /dev/null; then
            missing_tools+=("${tool}")
        fi
    done
    
    if [[ ${#missing_tools[@]} -gt 0 ]]; then
        kuasar::log::error "Missing required tools: ${missing_tools[*]}"
        return 1
    fi
    
    return 0
}

# kuasar::util::detect_arch detects the system architecture  
function kuasar::util::detect_arch() {
    local arch
    case "$(uname -m)" in
        x86_64)
            arch="x86_64"
            ;;
        aarch64|arm64)
            arch="aarch64"
            ;;
        *)
            kuasar::log::error "Unsupported architecture: $(uname -m)"
            return 1
            ;;
    esac
    echo "${arch}"
}

# kuasar::util::detect_os detects the operating system
function kuasar::util::detect_os() {
    local os
    case "$(uname -s)" in
        Linux)
            os="linux"
            ;;
        Darwin)
            os="macos"
            ;;
        *)
            kuasar::log::error "Unsupported operating system: $(uname -s)"
            return 1
            ;;
    esac
    echo "${os}"
}

# kuasar::util::get_git_version gets the Git version information
function kuasar::util::get_git_version() {
    local git_version
    if git_version="$(git describe --tags --dirty --always 2>/dev/null)"; then
        echo "${git_version}"
    else
        echo "unknown"
    fi
}

# kuasar::util::print_separator prints a visual separator
function kuasar::util::print_separator() {
    local char="${1:-=}"
    local length="${2:-80}"
    printf "%*s\n" "${length}" "" | tr ' ' "${char}"
}

# kuasar::util::retry retries a command with exponential backoff
function kuasar::util::retry() {
    local max_attempts="$1"
    local delay="$2"
    shift 2
    
    local attempt=1
    while [[ ${attempt} -le ${max_attempts} ]]; do
        if "$@"; then
            return 0
        fi
        
        if [[ ${attempt} -lt ${max_attempts} ]]; then
            kuasar::log::warn "Command failed (attempt ${attempt}/${max_attempts}), retrying in ${delay}s..."
            sleep "${delay}"
            delay=$((delay * 2))
        fi
        
        attempt=$((attempt + 1))
    done
    
    kuasar::log::error "Command failed after ${max_attempts} attempts"
    return 1
}
