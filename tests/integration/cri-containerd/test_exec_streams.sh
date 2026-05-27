#!/usr/bin/env bash

# This script is sourced by integration-tests.sh and relies on its variables and helper functions.

wait_for_log_seq_gt() {
    local cid="$1"
    local min_seq="${2:--1}"
    local retries="${3:-20}"
    local delay="${4:-0.5}"
    local seq=-1

    for _ in $(seq 1 "${retries}"); do
        seq="$(
            sudo crictl logs "${cid}" 2>/dev/null | \
                awk '/^SEQ[[:space:]][0-9]+([[:space:]]|$)/ { seq=$2 } END { print (seq == "" ? -1 : seq) }'
        )"
        if [[ ${seq} -gt ${min_seq} ]]; then
            printf '%s\n' "${seq}"
            return 0
        fi
        sleep "${delay}"
    done

    printf '%s\n' "${seq}"
    return 1
}

# Verify IOChannel recovery and no panics when client disconnects (graceful exit vs abrupt kill).
# 1. Start crictl exec producing continuous output.
# 2. Loop 50 times, alternating between graceful exit and abrupt client kill.
# 3. Check sandbox stability to ensure subsequent execs respond normally.
test_exec_stream_disconnect_loop() {
    local max_iters=50
    local log_dir="${ARTIFACTS_DIR}/exec_disconnect"
    mkdir -p "${log_dir}"
    
    log "Running crictl exec disconnect loop for ${max_iters} iterations..."
    
    for i in $(seq 1 "${max_iters}"); do
        if (( i % 5 == 0 )); then
            # Graceful exit: command finishes by itself
            sudo crictl exec "${container_id}" sh -c "echo 'graceful'; sleep 0.2" > "${log_dir}/exec_${i}.log" 2>&1
        else
            # Abrupt kill: kill the crictl exec process while it's receiving output
            sudo crictl exec "${container_id}" sh -c "while true; do echo 'alive'; sleep 0.05; done" > "${log_dir}/exec_${i}.log" 2>&1 &
            local exec_pid=$!
            sleep 0.3
            sudo kill -9 "${exec_pid}" >/dev/null 2>&1 || true
            wait "${exec_pid}" 2>/dev/null || true
        fi
    done
    
    local ping_out
    ping_out=$(sudo crictl exec "${container_id}" sh -c 'echo pong' 2>&1)
    [[ "${ping_out}" == "pong" ]] || die "Sandbox unstable after disconnect loop: got ${ping_out}"
}

# Verify stdout draining completes before process exit for fast-exiting tasks.
# 1. Generate 10MB of deterministic data using awk and exit immediately.
# 2. Redirect output to a file on the host.
# 3. Validate file size (exactly 10MB) and SHA256 hash integrity.
test_exec_exit_drain_stdout() {
    local out_file="${ARTIFACTS_DIR}/drain_stdout.bin"
    
    log "Testing stdout draining upon fast process exit"
    
    sudo crictl exec "${container_id}" sh -c 'awk "BEGIN{for(i=0;i<10240;i++) printf \"%1024s\", \"\" | \"tr \\\" \\\" \\\"A\\\"\"}"' > "${out_file}"
    
    local file_size
    file_size=$(stat -c %s "${out_file}")
    [[ "${file_size}" -eq 10485760 ]] || die "Stdout drain failed, expected 10485760 bytes, got ${file_size}"
    
    local hash_val
    hash_val=$(sha256sum "${out_file}" | awk '{print $1}')
    local expected_hash="eb6183addde05c2196ce25e6fa34a4baf20f9bf30d33892f452a9a1e88c9a472"
    [[ "${hash_val}" == "${expected_hash}" ]] || die "Stdout drain hash mismatch"
}

# Verify stderr draining completes before process exit to ensure API refactoring coverage.
# 1. Direct 10MB test data to stderr and exit immediately.
# 2. Capture stderr on the host using 2>&1 redirection.
# 3. Validate the consistency of the received file size.
test_exec_exit_drain_stderr() {
    local out_file="${ARTIFACTS_DIR}/drain_stderr.bin"
    
    log "Testing stderr draining upon fast process exit"
    
    sudo crictl exec "${container_id}" sh -c 'awk "BEGIN{for(i=0;i<10240;i++) printf \"%1024s\", \"\" | \"tr \\\" \\\" \\\"B\\\"\"}" >&2' > "${out_file}" 2>&1
    
    local file_size
    file_size=$(stat -c %s "${out_file}")
    [[ "${file_size}" -eq 10485760 ]] || die "Stderr drain failed, expected 10485760 bytes, got ${file_size}"
    
    local hash_val
    hash_val=$(sha256sum "${out_file}" | awk '{print $1}')
    local expected_hash="4206ae362958087f93cacff490e2922d285b5fa018faeaa13804e8b98ea36a6e"
    [[ "${hash_val}" == "${expected_hash}" ]] || die "Stderr drain hash mismatch"
}

# Verify StreamingStdin boundary handling (64KiB +/- 1) and chunk merging logic.
# 1. Prepare deterministic payloads with special sizes: 64KiB-1, 64KiB+1, 1MiB+17B.
# 2. Stream into the container via crictl exec -i.
# 3. Compare SHA256 hashes between host source and guest destination files.
test_exec_stdin_boundaries() {
    log "Testing StreamingStdin overlapping boundaries"
    
    local boundary_sizes=(65535 65537 1048593)
    
    for b_size in "${boundary_sizes[@]}"; do
        local in_file="${ARTIFACTS_DIR}/stdin_bound_${b_size}.bin"
        dd if=/dev/urandom of="${in_file}" bs=1 count="${b_size}" status=none
        local expected_hash
        expected_hash=$(sha256sum "${in_file}" | awk '{print $1}')
        
        sudo crictl exec -i "${container_id}" sh -c "cat > /tmp/out_${b_size}.bin" < "${in_file}"
        
        local guest_hash
        guest_hash=$(sudo crictl exec "${container_id}" sha256sum "/tmp/out_${b_size}.bin" | awk '{print $1}')
        
        [[ "${expected_hash}" == "${guest_hash}" ]] || die "Stdin boundary match failed for size ${b_size}: host ${expected_hash}, guest ${guest_hash}"
    done
}

# Verify that leftover data in the Stdin buffer is drained correctly upon EOF.
# 1. Send a string to the container and close the input pipe.
# 2. Verify the guest process reads the full string rather than hanging on an incomplete chunk.
# 3. Implement a 5-second timeout as a safety guard.
test_exec_stdin_eof() {
    log "Testing stdin residual buffer draining on EOF"
    
    local stdout_file="${ARTIFACTS_DIR}/stdin_eof_out.txt"
    printf %s "kuasar-eof-test" | sudo timeout 5s crictl exec -i "${container_id}" cat > "${stdout_file}"
    local rc=$?
    
    [[ ${rc} -eq 0 ]] || die "crictl exec timeout or failed with rc=${rc} when testing EOF"
    
    local output
    output=$(cat "${stdout_file}")
    [[ "${output}" == "kuasar-eof-test" ]] || die "EOF leftover buffering failed, got: ${output}"
}

# Verify that container stdout logging resumes correctly after the containerd service restarts.
# 1. Start a container outputting timestamps and verify initial log rolling.
# 2. Force kill and restart the containerd daemon.
# 3. Assert that crictl logs -f continues to receive new log lines after reconciliation.
test_containerd_restart_log_resume() {
    log "Testing containerd restart logging continuity"

    local c_config="${ARTIFACTS_DIR}/log-test.json"
    local p_config="${ARTIFACTS_DIR}/podsandbox.json"
    local inspect_file="${ARTIFACTS_DIR}/failed_inspect_log_resume.json"
    local stalled_logs_file="${ARTIFACTS_DIR}/stalled_logs_log_resume.txt"
    
    create_podsandbox_config "${p_config}"
    cat > "${c_config}" <<'EOFCFG'
{
  "metadata": { "name": "log-resume", "namespace": "default" },
  "image": { "image": "docker.io/library/busybox:1.36.1" },
  "command": ["sh", "-c", "i=0; while true; do echo \"SEQ $i $(date)\"; i=$((i+1)); sleep 0.2; done"],
  "log_path": "log.txt",
  "linux": { "security_context": { "namespace_options": { "network": 2 } } }
}
EOFCFG

    local cid
    local seq_before
    local seq_after
    cid="$(sudo crictl create "${pod_id}" "${c_config}" "${p_config}")"
    sudo crictl start "${cid}"

    if ! seq_before="$(wait_for_log_seq_gt "${cid}")"; then
        log "DIAGNOSTIC: No initial SEQ log lines found for ${cid}. Inspecting..."
        sudo crictl inspect "${cid}" > "${inspect_file}" 2>&1 || true
        sudo crictl ps -a --id "${cid}" >> "${inspect_file}" 2>&1 || true
        die "No initial sequenced logs found for container ${cid}"
    fi

    log "Restarting containerd..."
    [[ -f "${containerd_pid_file}" ]] && sudo kill -9 "$(cat "${containerd_pid_file}")" >/dev/null 2>&1 || true
    sleep 1
    start_containerd

    if ! seq_after="$(wait_for_log_seq_gt "${cid}" "${seq_before}")"; then
        log "DIAGNOSTIC: Logs stalled. Before seq: ${seq_before}, After seq: ${seq_after}. Logs dump:"
        sudo crictl logs "${cid}" >> "${stalled_logs_file}" 2>&1 || true
        die "Logs did not advance after containerd restart: before_seq=${seq_before}, after_seq=${seq_after}"
    fi

    log "Logs resumed properly (before seq: ${seq_before}, after seq: ${seq_after})"
    sudo crictl rm -f "${cid}" >/dev/null 2>&1 || true
}
