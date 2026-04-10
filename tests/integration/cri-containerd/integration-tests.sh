#!/usr/bin/env bash

set -o errexit
set -o nounset
set -o pipefail

readonly script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
readonly repo_root="$(cd "${script_dir}/../../.." && pwd -P)"
readonly report_dir="$(mktemp -d -t kuasar-cri-containerd.XXXX)"

if ! command -v sudo >/dev/null 2>&1; then
    sudo() {
        "$@"
    }
    export -f sudo
fi

export HYPERVISOR="${HYPERVISOR:-clh}"
export RUNTIME_HANDLER="${RUNTIME_HANDLER:-kuasar-vmm}"
export ARTIFACTS_DIR="${ARTIFACTS_DIR:-${repo_root}/_artifacts/vmm-${HYPERVISOR}-cri-containerd}"
export TEST_FILTER="${TEST_FILTER:-}"

readonly containerd_log="${ARTIFACTS_DIR}/containerd.log"
readonly vmm_log="${ARTIFACTS_DIR}/vmm-sandboxer.log"
readonly cloud_hypervisor_ps="${ARTIFACTS_DIR}/cloud-hypervisor.ps.txt"
readonly crictl_info="${ARTIFACTS_DIR}/crictl-info.txt"
readonly crictl_pods="${ARTIFACTS_DIR}/crictl-pods.txt"
readonly crictl_ps="${ARTIFACTS_DIR}/crictl-ps.txt"
readonly sandbox_inspect="${ARTIFACTS_DIR}/sandbox-inspect.json"
readonly container_inspect="${ARTIFACTS_DIR}/container-inspect.json"
readonly config_snapshot="${ARTIFACTS_DIR}/config.toml"
readonly exec_output_file="${ARTIFACTS_DIR}/exec-output.txt"
readonly kuasar_ctl_output_file="${ARTIFACTS_DIR}/kuasar-ctl-output.txt"
readonly task_logs_dir="${ARTIFACTS_DIR}/task-logs"

readonly containerd_pid_file="${ARTIFACTS_DIR}/containerd.pid"
readonly vmm_pid_file="${ARTIFACTS_DIR}/vmm-sandboxer.pid"
readonly cni_config_file="/etc/cni/net.d/10-kuasar.conflist"
readonly crictl_config_file="/etc/crictl.yaml"
readonly kuasar_config_file="/var/lib/kuasar/config.toml"
readonly vmm_socket="/run/vmm-sandboxer.sock"
readonly tap_file="${ARTIFACTS_DIR}/results.tap"
readonly recovery_startup_budget_secs="${RECOVERY_STARTUP_BUDGET_SECS:-10}"

export BUSYBOX_IMAGE="${BUSYBOX_IMAGE:-docker.io/library/busybox:1.36.1}"

pod_id=""
container_id=""
test_failures=()
error_reported=0
extra_pod_ids=()
tap_count=0
recovery_elapsed_secs=0

log() {
    printf '[cri-containerd] %s\n' "$*"
}

die() {
    printf '[cri-containerd] ERROR: %s\n' "$*" >&2
    exit 1
}

dump_recent_logs() {
    local task_logs=()
    local primary_task_log=""
    local task_log=""

    echo '::group::containerd log (last 100 lines)'
    if [[ -f "${containerd_log}" ]]; then
        tail -n 100 "${containerd_log}" || true
    else
        printf '[cri-containerd] containerd log not found: %s\n' "${containerd_log}"
    fi
    echo '::endgroup::'

    echo '::group::vmm-sandboxer log (last 200 lines)'
    if [[ -f "${vmm_log}" ]]; then
        tail -n 200 "${vmm_log}" || true
    else
        printf '[cri-containerd] vmm-sandboxer log not found: %s\n' "${vmm_log}"
    fi
    echo '::endgroup::'

    if [[ -n "${pod_id}" ]]; then
        primary_task_log="/tmp/${pod_id}-task.log"
    fi

    echo "::group::vmm-task logs (last 200 lines)"
    if [[ -n "${primary_task_log}" ]] && [[ -f "${primary_task_log}" ]]; then
        printf '[cri-containerd] tailing primary task log: %s\n' "${primary_task_log}"
        tail -n 200 "${primary_task_log}" || true
    fi

    mapfile -t task_logs < <(compgen -G '/tmp/*-task.log' | sort || true)
    if [[ ${#task_logs[@]} -eq 0 ]]; then
        printf '[cri-containerd] no vmm-task log found under /tmp\n'
    fi

    for task_log in "${task_logs[@]}"; do
        if [[ -n "${primary_task_log}" ]] && [[ "${task_log}" == "${primary_task_log}" ]]; then
            continue
        fi
        printf '[cri-containerd] tailing fallback task log: %s\n' "${task_log}"
        tail -n 200 "${task_log}" || true
    done
    echo '::endgroup::'
}

report_failure() {
    local exit_code="${1:-1}"

    if [[ "${error_reported}" -eq 1 ]]; then
        return
    fi
    error_reported=1

    dump_recent_logs
    log "command failed with exit code ${exit_code}"
}

get_matching_pids() {
    local process_name="$1"

    pgrep -x "${process_name}" 2>/dev/null | sort || true
}

wait_for_file() {
    local target="$1"
    local retries="${2:-30}"
    local delay="${3:-1}"

    for _ in $(seq 1 "${retries}"); do
        if [[ -e "${target}" ]]; then
            return 0
        fi
        sleep "${delay}"
    done

    return 1
}

wait_for_container_state() {
    local retries="${1:-30}"
    local delay="${2:-2}"

    for _ in $(seq 1 "${retries}"); do
        local state
        state="$(sudo crictl inspect "${container_id}" 2>/dev/null | jq -r '.status.state // empty' || true)"
        if [[ "${state}" == "CONTAINER_RUNNING" ]]; then
            return 0
        fi
        sleep "${delay}"
    done

    return 1
}

wait_for_pod_state() {
    local target_pod_id="$1"
    local expected_state="$2"
    local retries="${3:-30}"
    local delay="${4:-1}"
    local state=""

    for _ in $(seq 1 "${retries}"); do
        state="$(sudo crictl inspectp "${target_pod_id}" 2>/dev/null | jq -r '.status.state // empty' || true)"
        if [[ "${state}" == "${expected_state}" ]]; then
            return 0
        fi
        sleep "${delay}"
    done

    log "pod ${target_pod_id} state did not reach ${expected_state}, last state: ${state}"
    return 1
}

wait_for_pod_removed() {
    local target_pod_id="$1"
    local retries="${2:-30}"
    local delay="${3:-1}"

    for _ in $(seq 1 "${retries}"); do
        if ! sudo crictl inspectp "${target_pod_id}" >/dev/null 2>&1; then
            return 0
        fi
        sleep "${delay}"
    done

    return 1
}

wait_for_sandbox_dir_removed() {
    local target_pod_id="$1"
    local retries="${2:-30}"
    local delay="${3:-1}"

    for _ in $(seq 1 "${retries}"); do
        if [[ ! -d "/run/kuasar-vmm/${target_pod_id}" ]]; then
            return 0
        fi
        sleep "${delay}"
    done

    return 1
}

get_vm_pid() {
    local p_id="$1"
    local process_name

    case "${HYPERVISOR}" in
        clh)
            process_name="cloud-hypervisor"
            ;;
        qemu)
            process_name="qemu-system"
            ;;
        stratovirt)
            process_name="stratovirt"
            ;;
        *)
            process_name="${HYPERVISOR}"
            ;;
    esac

    pgrep -f "${process_name}.*${p_id:0:12}" | grep -v "sandboxer" | head -1
}

resume_vm_if_alive() {
    local vm_pid="$1"

    if sudo kill -0 "${vm_pid}" >/dev/null 2>&1; then
        sudo kill -CONT "${vm_pid}" || true
    fi
}

wait_for_recovery_start() {
    local target_pod_id="$1"
    local retries="${2:-30}"
    local delay="${3:-0.2}"

    for _ in $(seq 1 "${retries}"); do
        if [[ -f "${vmm_log}" ]] && grep -Fq "recovering sandbox \"${target_pod_id}\"" "${vmm_log}"; then
            return 0
        fi
        sleep "${delay}"
    done

    return 1
}

create_container_config() {
    local container_config="$1"
    local name="${2:-kuasar-vmm-busybox}"
    local log_path="${3:-busybox.log}"

    cat >"${container_config}" <<EOF
{
  "metadata": {
    "name": "${name}",
    "namespace": "default"
  },
  "image": {
    "image": "${BUSYBOX_IMAGE}"
  },
  "command": [
    "/bin/sh",
    "-c",
    "sleep 3600"
  ],
  "log_path": "${log_path}",
  "linux": {
    "security_context": {
      "namespace_options": {
        "network": 2,
        "pid": 1
      }
    }
  }
}
EOF
}

create_podsandbox_config() {
    local pod_config="$1"
    local name="${2:-kuasar-vmm-clh-sandbox}"
    local uid="${3:-kuasar-vmm-clh-sandbox-uid}"

    cat >"${pod_config}" <<EOF
{
  "metadata": {
    "name": "${name}",
    "namespace": "default",
    "uid": "${uid}"
  },
  "log_directory": "/tmp",
  "linux": {
    "security_context": {
      "namespace_options": {
        "network": 2,
        "pid": 1
      }
    }
  }
}
EOF
}

write_crictl_config() {
    sudo tee "${crictl_config_file}" >/dev/null <<'EOF'
runtime-endpoint: "unix:///run/containerd/containerd.sock"
image-endpoint: "unix:///run/containerd/containerd.sock"
timeout: 60
debug: false
pull-image-on-create: true
EOF
}

write_cni_config() {
    sudo mkdir -p "$(dirname "${cni_config_file}")"
    sudo tee "${cni_config_file}" >/dev/null <<'EOF'
{
  "cniVersion": "0.4.0",
  "name": "kuasar-test",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "cni0",
      "isGateway": true,
      "ipMasq": true,
      "promiscMode": true,
      "ipam": {
        "type": "host-local",
        "ranges": [
          [
            {
              "subnet": "10.88.0.0/16"
            }
          ]
        ],
        "routes": [
          {
            "dst": "0.0.0.0/0"
          }
        ]
      }
    },
    {
      "type": "portmap",
      "capabilities": {
        "portMappings": true
      }
    }
  ]
}
EOF
}

start_containerd() {
    if [[ -e "/run/containerd/containerd.sock" ]] && pgrep -x containerd >/dev/null; then
        log "containerd is already running"
        return
    fi

    sudo rm -f /run/containerd/containerd.sock || true
    sudo mkdir -p /run/containerd
    sudo bash -c "ENABLE_CRI_SANDBOXES=1 /usr/local/bin/containerd --config /etc/containerd/config.toml > '${containerd_log}' 2>&1 & echo \$! > '${containerd_pid_file}'"

    wait_for_file "/run/containerd/containerd.sock" 60 1 || die "containerd socket did not appear"
}

start_vmm_sandboxer() {
    if [[ -e "${vmm_socket}" ]] && pgrep -x vmm-sandboxer >/dev/null; then
        log "vmm-sandboxer is already running"
        return
    fi

    sudo rm -f "${vmm_socket}" || true
    sudo mkdir -p /run/kuasar-vmm
    # Pass the installed config path explicitly so CI does not rely on the
    # sandboxer's default config resolution behavior.
    sudo bash -c "/usr/local/bin/vmm-sandboxer --config ${kuasar_config_file} --listen ${vmm_socket} --dir /run/kuasar-vmm > '${vmm_log}' 2>&1 & echo \$! > '${vmm_pid_file}'"

    wait_for_file "${vmm_socket}" 60 1 || die "vmm-sandboxer socket did not appear"
}

collect_state() {
    local task_logs=()

    sudo crictl info >"${crictl_info}" 2>&1 || true
    sudo crictl pods >"${crictl_pods}" 2>&1 || true
    sudo crictl ps -a >"${crictl_ps}" 2>&1 || true

    if [[ -n "${pod_id}" ]]; then
        sudo crictl inspectp "${pod_id}" >"${sandbox_inspect}" 2>&1 || true
    fi

    if [[ -n "${container_id}" ]]; then
        sudo crictl inspect "${container_id}" >"${container_inspect}" 2>&1 || true
    fi

    pgrep -af cloud-hypervisor >"${cloud_hypervisor_ps}" 2>&1 || true
    sudo cp /etc/containerd/config.toml "${config_snapshot}" 2>/dev/null || true

    mkdir -p "${task_logs_dir}"
    mapfile -t task_logs < <(compgen -G '/tmp/*-task.log' | sort || true)
    for task_log in "${task_logs[@]}"; do
        cp "${task_log}" "${task_logs_dir}/$(basename "${task_log}")" 2>/dev/null || true
    done
}

cleanup() {
    set +e
    local extra_pod_id

    collect_state

    if [[ -n "${container_id}" ]]; then
        sudo crictl rm -f "${container_id}" >/dev/null 2>&1 || true
    fi

    if [[ -n "${pod_id}" ]]; then
        sudo crictl stopp "${pod_id}" >/dev/null 2>&1 || true
        sudo crictl rmp -f "${pod_id}" >/dev/null 2>&1 || true
    fi

    for extra_pod_id in "${extra_pod_ids[@]}"; do
        sudo crictl stopp "${extra_pod_id}" >/dev/null 2>&1 || true
        sudo crictl rmp -f "${extra_pod_id}" >/dev/null 2>&1 || true
    done

    if [[ -f "${vmm_pid_file}" ]]; then
        sudo kill "$(cat "${vmm_pid_file}")" >/dev/null 2>&1 || true
    fi

    if [[ -f "${containerd_pid_file}" ]]; then
        sudo kill "$(cat "${containerd_pid_file}")" >/dev/null 2>&1 || true
    fi

    rm -rf "${report_dir}" >/dev/null 2>&1 || true
}

trap cleanup EXIT
trap 'report_failure $?' ERR

setup_sandbox() {
    log "Setting up shared sandbox fixture"
    create_podsandbox_config "${report_dir}/podsandbox.json"
    create_container_config "${report_dir}/container.json"

    sudo crictl pull "${BUSYBOX_IMAGE}"

    pod_id="$(sudo crictl runp --runtime="${RUNTIME_HANDLER}" "${report_dir}/podsandbox.json")"
    container_id="$(sudo crictl create "${pod_id}" "${report_dir}/container.json" "${report_dir}/podsandbox.json")"
    sudo crictl start "${container_id}"

    wait_for_container_state 30 2 || die "container did not enter running state"

    log "Verifying Cloud Hypervisor instance"
    local clh_cmdline
    clh_cmdline="$(pgrep -af cloud-hypervisor 2>/dev/null || true)"
    [[ -n "${clh_cmdline}" ]] || die "cloud-hypervisor process not found"
    echo "${clh_cmdline}" | grep -q "/var/lib/kuasar/vmlinux.bin" || die "cloud-hypervisor command line missing kernel path"
    echo "${clh_cmdline}" | grep -q "/var/lib/kuasar/kuasar.img" || die "cloud-hypervisor command line missing image path"
}

test_multi_container_pod() {
    local container2_config="${report_dir}/container2.json"
    local container2_id
    local output

    log "Adding second container to shared sandbox"
    create_container_config "${container2_config}" "kuasar-vmm-busybox-2" "busybox-2.log"

    container2_id="$(sudo crictl create "${pod_id}" "${container2_config}" "${report_dir}/podsandbox.json")"
    sudo crictl start "${container2_id}"

    # Verify both can exec
    output="$(sudo crictl exec "${container_id}" /bin/sh -c 'echo c1-ok')"
    [[ "${output}" == "c1-ok" ]] || die "container 1 exec failed in multi-container pod"

    output="$(sudo crictl exec "${container2_id}" /bin/sh -c 'echo c2-ok')"
    [[ "${output}" == "c2-ok" ]] || die "container 2 exec failed in multi-container pod"

    # Cleanup the second container
    sudo crictl rm -f "${container2_id}"
}

test_cri_pod_lifecycle() {
    local pod_cfg="${report_dir}/lifecycle-pod.json"
    local p_id
    local state

    # Create a separate pod for lifecycle test
    create_podsandbox_config "${pod_cfg}" "lifecycle-test-pod" "lifecycle-uid"

    log "Testing Pod creation"
    p_id="$(sudo crictl runp --runtime="${RUNTIME_HANDLER}" "${pod_cfg}")"
    [[ -n "${p_id}" ]] || die "failed to run pod"
    extra_pod_ids+=("${p_id}")

    # Verify Ready
    state="$(sudo crictl inspectp "${p_id}" | jq -r '.status.state')"
    [[ "${state}" == "SANDBOX_READY" ]] || die "expected pod to be READY, got ${state}"

    log "Testing Pod stopping"
    sudo crictl stopp "${p_id}"
    state="$(sudo crictl inspectp "${p_id}" | jq -r '.status.state')"
    [[ "${state}" == "SANDBOX_NOTREADY" ]] || die "expected pod to be NOTREADY, got ${state}"

    log "Testing Pod removal"
    sudo crictl rmp -f "${p_id}"
    if sudo crictl inspectp "${p_id}" >/dev/null 2>&1; then
        die "pod ${p_id} still exists after rmp"
    fi
    extra_pod_ids=("${extra_pod_ids[@]/$p_id/}")
}

test_exec() {
    local exec_output

    set +e
    exec_output="$(sudo crictl exec "${container_id}" /bin/sh -c 'echo kuasar-e2e-ok' 2>&1)"
    local exec_rc=$?
    set -e

    printf '%s\n' "${exec_output}" >"${exec_output_file}"
    [[ ${exec_rc} -eq 0 ]] || die "crictl exec failed: ${exec_output}"
    grep -q "kuasar-e2e-ok" "${exec_output_file}" || die "unexpected exec output: ${exec_output}"
}

test_kuasar_ctl_exec() {
    local output

    # Use prefix of pod_id for resolution check
    output="$(sudo kuasar-ctl exec "${pod_id:0:12}" -- sh -c 'echo kuasar-ctl-ok')"
    printf '%s\n' "${output}" >"${kuasar_ctl_output_file}"
    grep -q "kuasar-ctl-ok" <<< "${output}" || die "kuasar-ctl exec output mismatch: ${output}"
}

test_kuasar_ctl_exit_code() {
    local rc=0
    set +e
    sudo kuasar-ctl exec "${pod_id:0:12}" -- sh -c 'exit 42'
    rc=$?
    set -e
    [[ ${rc} -eq 42 ]] || die "kuasar-ctl exit code passthrough failed: got ${rc}, want 42"
}

test_kuasar_ctl_timeout() {
    local rc=0
    set +e
    sudo kuasar-ctl exec "${pod_id:0:12}" --timeout 2 -- sh -c 'cat'
    rc=$?
    set -e
    [[ ${rc} -eq 124 ]] || die "kuasar-ctl timeout exit code mismatch: got ${rc}, want 124"
}

test_kuasar_ctl_no_fd_leak() {
    local vmm_pid clh_pid
    vmm_pid=$(cat "${vmm_pid_file}")
    clh_pid=$(get_vm_pid "${pod_id}")

    # warmup: exclude initial connection setup
    sudo kuasar-ctl exec "${pod_id:0:12}" -- sh -c 'echo warmup' > /dev/null

    local vmm_fd_before clh_fd_before
    vmm_fd_before=$(sudo ls /proc/"${vmm_pid}"/fd 2>/dev/null | wc -l)
    clh_fd_before=$(sudo ls /proc/"${clh_pid}"/fd 2>/dev/null | wc -l)

    for i in $(seq 1 100); do
        sudo kuasar-ctl exec "${pod_id:0:12}" -- sh -c "echo fd-leak-test-${i}" > /dev/null &
    done
    wait
    sleep 1

    local vmm_fd_after clh_fd_after
    vmm_fd_after=$(sudo ls /proc/"${vmm_pid}"/fd 2>/dev/null | wc -l)
    clh_fd_after=$(sudo ls /proc/"${clh_pid}"/fd 2>/dev/null | wc -l)

    local vmm_diff clh_diff
    vmm_diff=$(( vmm_fd_after - vmm_fd_before ))
    clh_diff=$(( clh_fd_after - clh_fd_before ))

    # Allow small fluctuations but not linear growth
    [[ ${vmm_diff} -le 5 ]] || die "vmm-sandboxer fd leak: before=${vmm_fd_before} after=${vmm_fd_after} diff=${vmm_diff}"
    [[ ${clh_diff} -le 5 ]] || die "cloud-hypervisor fd leak: before=${clh_fd_before} after=${clh_fd_after} diff=${clh_diff}"
}

test_kuasar_ctl_no_proc_leak() {
    local pids_before
    local pids_after
    local lingering

    pids_before="$(get_matching_pids kuasar-ctl)"

    for i in $(seq 1 100); do
        sudo kuasar-ctl exec "${pod_id:0:12}" -- sh -c "echo proc-leak-test-${i}" > /dev/null &
    done
    wait

    sleep 1
    pids_after="$(get_matching_pids kuasar-ctl)"
    if [[ "${pids_before}" != "${pids_after}" ]]; then
        lingering="$(comm -13 <(printf '%s\n' "${pids_before}") <(printf '%s\n' "${pids_after}") | tr '\n' ' ')"
        die "lingering kuasar-ctl processes detected: ${lingering}"
    fi

    local zombies
    zombies=$(ps -o stat= --ppid $$ | grep -c '^Z' || true)
    [[ "${zombies}" -eq 0 ]] || die "zombie child processes detected for test shell: ${zombies}"
}

test_vmm_killed_cleanup() {
    local p_config="${report_dir}/kill-vmm-pod.json"
    local p_id
    local clh_pid

    log "Testing VMM killed cleanup"
    create_podsandbox_config "${p_config}" "kill-vmm-pod" "kill-vmm-uid"
    p_id="$(sudo crictl runp --runtime="${RUNTIME_HANDLER}" "${p_config}")"
    extra_pod_ids+=("${p_id}")

    # Find the CLH process for *this* pod specifically.
    clh_pid=$(get_vm_pid "${p_id}")
    [[ -n "${clh_pid}" ]] || die "could not find cloud-hypervisor process for pod ${p_id}"

    log "Killing Cloud Hypervisor (PID: ${clh_pid}) for Pod ${p_id}"
    sudo kill -9 "${clh_pid}"

    # Wait for vmm-sandboxer/containerd to realize the VMM is gone
    sleep 5

    # Attempt to stop and remove. It should not hang.
    sudo crictl stopp "${p_id}" >/dev/null 2>&1 || true
    sudo crictl rmp -f "${p_id}" >/dev/null 2>&1 || true

    # Clean from tracker
    extra_pod_ids=("${extra_pod_ids[@]/$p_id/}")

    # Validate that no shim or clh process is left for this pod
    if pgrep -f "${p_id}" >/dev/null 2>&1; then
        die "processes for pod ${p_id} still lingering after VMM killed"
    fi
}


# Shared setup for recovery tests: create pod, find VM PID, suspend VM,
# restart sandboxer, and record how long it takes to become available again.
# Sets caller variables: p_id, vm_pid.
setup_recovery_test() {
    local test_name="$1"
    local p_config="${report_dir}/${test_name}.json"
    local recovery_start

    create_podsandbox_config "${p_config}" "${test_name}" "${test_name}-uid"
    p_id="$(sudo crictl runp --runtime="${RUNTIME_HANDLER}" "${p_config}")"
    extra_pod_ids+=("${p_id}")

    vm_pid=$(get_vm_pid "${p_id}")
    [[ -n "${vm_pid}" ]] || die "could not find VM process for pod ${p_id}"

    log "Suspending VM (PID: ${vm_pid})"
    sudo kill -STOP "${vm_pid}"

    log "Restarting vmm-sandboxer"
    sudo kill "$(cat "${vmm_pid_file}")" || true
    recovery_start=${SECONDS}
    start_vmm_sandboxer
    recovery_elapsed_secs=$((SECONDS - recovery_start))

    wait_for_recovery_start "${p_id}" || die "sandboxer did not start recovery for pod ${p_id}"
}

test_recovery_slow_agent_success() {
    local p_id
    local vm_pid
    local state

    log "Testing Sandbox recovery with injected VMM stall (service recovery case)"
    setup_recovery_test "recovery-slow-success"

    # Safety: ensure VM is resumed even if we die mid-test.
    trap 'resume_vm_if_alive "${vm_pid}"' RETURN

    [[ ${recovery_elapsed_secs} -le ${recovery_startup_budget_secs} ]] || die \
        "vmm-sandboxer recovery exceeded budget: ${recovery_elapsed_secs}s > ${recovery_startup_budget_secs}s"

    log "Resuming VM (PID: ${vm_pid})"
    resume_vm_if_alive "${vm_pid}"

    log "Waiting for pod ${p_id} to settle into NOTREADY after fast-fail recovery"
    wait_for_pod_state "${p_id}" "SANDBOX_NOTREADY" 30 1 || die "pod did not become NOTREADY after recovery stall"
    state="$(sudo crictl inspectp "${p_id}" | jq -r '.status.state // empty')"
    log "Pod settled in state ${state}; vmm-sandboxer recovered in ${recovery_elapsed_secs}s"

    grep -Fq "failed to recover sandbox \"${p_id}\"" "${vmm_log}" || log "WARNING: could not find failure log for ${p_id}"
    grep -Fq "recover sandboxes finished" "${vmm_log}" || log "WARNING: could not find recovery summary log"

    # Cleanup
    sudo crictl stopp "${p_id}" >/dev/null 2>&1 || true
    sudo crictl rmp -f "${p_id}" >/dev/null 2>&1 || true
    extra_pod_ids=("${extra_pod_ids[@]/$p_id/}")

    trap - RETURN
}

test_recovery_slow_agent_timeout_fail() {
    local p_id
    local vm_pid

    log "Testing Sandbox recovery with injected VMM stall (cleanup case)"
    setup_recovery_test "recovery-slow-fail"

    # Safety: ensure VM is resumed even if we die mid-test.
    trap 'resume_vm_if_alive "${vm_pid}"' RETURN

    [[ ${recovery_elapsed_secs} -le ${recovery_startup_budget_secs} ]] || die \
        "vmm-sandboxer recovery exceeded budget: ${recovery_elapsed_secs}s > ${recovery_startup_budget_secs}s"

    log "Resuming VM (PID: ${vm_pid})"
    resume_vm_if_alive "${vm_pid}"

    log "Verifying pod ${p_id} stays NOTREADY and sandbox runtime state is cleaned up"
    wait_for_pod_state "${p_id}" "SANDBOX_NOTREADY" 30 1 || die "pod ${p_id} did not become NOTREADY after recovery timeout"
    wait_for_sandbox_dir_removed "${p_id}" 10 1 || die "sandbox runtime dir for ${p_id} still exists after recovery timeout"

    # Verify the sandboxer logged a timeout error for this pod.
    grep -Fq "failed to recover sandbox \"${p_id}\"" "${vmm_log}" || log "WARNING: could not find failure log for ${p_id}"

    # Verify no lingering VM process for this pod.
    sleep 1
    if get_vm_pid "${p_id}" >/dev/null 2>&1; then
        log "WARNING: VM process for pod ${p_id} still exists after recovery cleanup"
    fi

    # Cleanup just in case
    sudo crictl rmp -f "${p_id}" >/dev/null 2>&1 || true
    extra_pod_ids=("${extra_pod_ids[@]/$p_id/}")

    trap - RETURN
}

run_test_group() {
    local group_name="$1"
    shift
    local tests=("$@")

    log ">>> Starting test group: ${group_name}"
    for test_name in "${tests[@]}"; do
        run_test_case "${test_name}"
    done
    log "<<< Finished test group: ${group_name}"
}

run_test_case() {
    local test_name="$1"

    if [[ -n "${TEST_FILTER}" ]] && [[ "${test_name}" != *"${TEST_FILTER}"* ]]; then
        return 0
    fi

    tap_count=$((tap_count + 1))
    log "Running ${test_name}"
    if "${test_name}"; then
        log "${test_name} passed"
        printf 'ok %d - %s\n' "${tap_count}" "${test_name}" | tee -a "${tap_file}"
        return 0
    fi

    log "${test_name} FAILED — dumping logs"
    printf 'not ok %d - %s\n' "${tap_count}" "${test_name}" | tee -a "${tap_file}"
    dump_recent_logs
    test_failures+=("${test_name}")
    return 1
}

run_all_tests() {
    run_test_group "CRI Basic" \
        test_cri_pod_lifecycle \
        test_multi_container_pod \
        test_exec

    run_test_group "kuasar-ctl Functionality" \
        test_kuasar_ctl_exec \
        test_kuasar_ctl_exit_code \
        test_kuasar_ctl_timeout

    run_test_group "kuasar-ctl Stability (Stress)" \
        test_kuasar_ctl_no_fd_leak \
        test_kuasar_ctl_no_proc_leak \
        test_vmm_killed_cleanup

    run_test_group "Recovery Robustness" \
        test_recovery_slow_agent_success \
        test_recovery_slow_agent_timeout_fail

    if [[ ${#test_failures[@]} -ne 0 ]]; then
        echo "1..${tap_count}" >> "${tap_file}"
        die "CRI tests failed: ${test_failures[*]}"
    fi
    echo "1..${tap_count}" >> "${tap_file}"
}

main() {
    mkdir -p "${ARTIFACTS_DIR}"
    echo "TAP version 13" > "${tap_file}"
    write_crictl_config
    write_cni_config
    start_containerd
    start_vmm_sandboxer
    if [[ -z "${TEST_FILTER}" ]] || [[ "${TEST_FILTER}" != *"recovery"* ]]; then
        setup_sandbox
    fi
    run_all_tests
    collect_state
    log "CRI tests passed for ${HYPERVISOR}"
}

main "$@"
