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

# Common initialization and utility functions for Kuasar hack scripts.
# This is modeled after Kubernetes' hack/lib/init.sh

set -o errexit
set -o nounset
set -o pipefail

# Detect the absolute path of the kuasar repository root
KUASAR_ROOT="${KUASAR_ROOT:-}"
if [[ -z "${KUASAR_ROOT}" ]]; then
    KUASAR_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
fi

# Source utility functions
source "${KUASAR_ROOT}/hack/lib/util.sh"

# Setup common environment
export KUASAR_ROOT

# Color output functions
readonly KUASAR_GREEN=$'\033[0;32m'
readonly KUASAR_RED=$'\033[0;31m'
readonly KUASAR_YELLOW=$'\033[0;33m'
readonly KUASAR_BLUE=$'\033[0;34m'
readonly KUASAR_RESET=$'\033[0m'

function kuasar::log::info() {
    echo "${KUASAR_BLUE}[INFO]${KUASAR_RESET} $*"
}

function kuasar::log::warn() {
    echo "${KUASAR_YELLOW}[WARN]${KUASAR_RESET} $*" >&2
}

function kuasar::log::error() {
    echo "${KUASAR_RED}[ERROR]${KUASAR_RESET} $*" >&2
}

function kuasar::log::success() {
    echo "${KUASAR_GREEN}[SUCCESS]${KUASAR_RESET} $*"
}

# Ensure we're in the repository root
cd "${KUASAR_ROOT}"
