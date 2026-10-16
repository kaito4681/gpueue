#!/usr/bin/env bash
# Real-hardware checkpoint/restore test against the gpueue INSTALLED on this
# host (daemon and helper under systemd), run as a normal user with no root.
# GPU checkpointing cannot be tested in a VM or in CI, so run this on a GPU
# host before every release (see "Testing checkpoint/restore on a GPU host"
# in docs/install.md).
#
# It builds two counter programs from scripts/gpu-host-test/ into a temporary
# directory: check.c keeps 4096 ints in GPU memory (CUDA driver API, inline
# PTX, libcuda loaded with dlopen, so only a C compiler and the NVIDIA driver
# are needed) and cpu.c keeps them in process memory. Both verify
# value[i] == step*(i+1) every second, write DIR/progress and DIR/history,
# and exit 0 once DIR/stop exists.
#
# Scenarios:
#   a  GPU job, 3 checkpoint/restore cycles
#   b  paused GPU job: pause, checkpoint, restore (comes back Paused), start
#   c  CPU job, 2 checkpoint/restore cycles
#   d  GPU job checkpointed while another GPU job runs and finishes, then
#      restored
#   e  GPU job with --no-checkpoint runs to Success
# After every dump: the job is Checkpointed with a size, its GPU slot is free,
# nvidia-smi shows none of its processes and its progress file stands still.
# After every restore: the counter continues from the dumped step (the
# history has every step exactly once). Every scenario ends by creating the
# stop file and expects the job to finish with Success.
#
# Usage: scripts/gpu-host-test.sh [--keep] [--only a,c] [--gpu N] [--group G]
#   --keep     do not kill or remove the jobs and keep the work directory
#   --only L   run only these scenarios (comma-separated letters)
#   --gpu N    the GPU to use (default 0)
#   --group G  the gpueue group to add the jobs to (default "default")
# Environment:
#   CC                  C compiler (default cc)
#   GPUEUE_TEST_TIMEOUT seconds to wait for a checkpoint or restore (default 300)
# Exits non-zero if the preflight or any scenario failed.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$root/scripts/gpu-host-test"

keep=0
only="a,b,c,d,e"
gpu=0
group=default
timeout="${GPUEUE_TEST_TIMEOUT:-300}"
cc="${CC:-cc}"

usage() { sed -n '/^# Usage:/,/^# Exits/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) keep=1 ;;
        --only) only="${2:?--only needs a list}"; shift ;;
        --only=*) only="${1#*=}" ;;
        --gpu) gpu="${2:?--gpu needs an index}"; shift ;;
        --gpu=*) gpu="${1#*=}" ;;
        --group) group="${2:?--group needs a name}"; shift ;;
        --group=*) group="${1#*=}" ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
case "$gpu" in '' | *[!0-9]*) echo "--gpu needs a number" >&2; exit 2 ;; esac
for s in ${only//,/ }; do
    case "$s" in a | b | c | d | e) ;; *) echo "unknown scenario: $s (a-e)" >&2; exit 2 ;; esac
done

run_id="ghtest-$(date +%Y%m%d%H%M%S)-$$"
since="$(date '+%Y-%m-%d %H:%M:%S')"
workdir=""
jobs_created=()   # every job of this run, for the cleanup
scenario_jobs=()  # the jobs of the current scenario, for failure details
results=()

say() { printf '%s\n' "$*"; }
ok() { printf '    ok    %s\n' "$*"; }
info() { printf '    ..    %s\n' "$*"; }
# The first failure of the current scenario, for the summary.
fail_reason=""
bad() {
    printf '    FAIL  %s\n' "$*"
    [ -n "$fail_reason" ] || fail_reason="$*"
}

# --- job helpers ------------------------------------------------------------

# The job's JSON object (empty when it does not exist).
job_json() { gpueue status --json 2>/dev/null | jq -c --argjson id "$1" '.jobs[] | select(.id == $id)'; }

# The job's state: running, paused, checkpointed, ..., done:<result>, and
# checkpointed:error for a Checkpointed job with a last_error (a failed
# restore), gone when the job does not exist.
job_state() {
    local j
    j="$(job_json "$1")"
    [ -n "$j" ] || { echo gone; return; }
    jq -r 'if .state.state == "done" then "done:" + .state.result
           elif .state.state == "checkpointed" and ((.details.last_error // "") != "")
           then "checkpointed:error" else .state.state end' <<<"$j"
}

last_error() { job_json "$1" | jq -r '.details.last_error // empty'; }

# wait_state ID TIMEOUT STATE...: wait until the job is in one of the states,
# or done, or gone (anything final). Prints the state it ended in; fails when
# that is not one of the wanted states.
wait_state() {
    local id="$1" limit="$2" st deadline want
    shift 2
    deadline=$((SECONDS + limit))
    while :; do
        st="$(job_state "$id")"
        for want in "$@"; do [ "$st" = "$want" ] && { echo "$st"; return 0; }; done
        case "$st" in done:* | gone | checkpointed:error) echo "$st"; return 1 ;; esac
        if [ "$SECONDS" -ge "$deadline" ]; then echo "$st (timed out after ${limit}s)"; return 1; fi
        sleep 1
    done
}

human() {
    awk -v b="$1" 'BEGIN { if (b >= 1048576) printf "%.1f MiB", b / 1048576; else printf "%.1f KiB", b / 1024 }'
}
progress_step() { awk '{print $1; exit}' "$1/progress" 2>/dev/null || echo 0; }
progress_step_or_zero() { local s; s="$(progress_step "$1")"; echo "${s:-0}"; }

# wait_progress DIR MIN TIMEOUT: wait until the counter reached step MIN.
wait_progress() {
    local dir="$1" min="$2" limit="$3" deadline
    deadline=$((SECONDS + limit))
    while [ "$(progress_step_or_zero "$dir")" -lt "$min" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 1
    done
}

# The history must hold the steps 1..N, each once and in order: a counter
# that restarted or skipped steps after a restore breaks this.
history_ok() {
    awk '$1 != NR { bad = 1; exit } END { exit bad || NR == 0 }' "$1/history" 2>/dev/null
}

# PIDs on the GPU of compute processes that run one of this run's programs.
job_gpu_pids() {
    nvidia-smi --query-compute-apps=pid,process_name --format=csv,noheader 2>/dev/null |
        awk -F', ' -v d="$workdir" 'index($2, d) { print $1 }'
}
gpu_slot_holds() {
    gpueue gpu --json 2>/dev/null |
        jq -e --argjson g "$gpu" --argjson id "$1" \
            '.gpus[] | select(.index == $g) | .jobs | map(.id) | index($id) != null' >/dev/null
}

# add_job NAME KIND DIR [gpueue add flags]: add a counter job and set new_id
# (not a command substitution: it records the job for the cleanup).
new_id=""
add_job() {
    local name="$1" kind="$2" dir="$3" prog
    shift 3
    prog="$workdir/bin/$kind"
    mkdir -p "$dir"
    local gpuflag=()
    [ "$kind" = check ] && gpuflag=(--gpu "$gpu")
    new_id="$(gpueue add -p -g "$group" -l "$run_id-$name" -w "$dir" "${gpuflag[@]}" "$@" -- \
        "exec '$prog' '$dir'")" || { bad "gpueue add failed"; return 1; }
    jobs_created+=("$new_id")
    scenario_jobs+=("$new_id")
}

# start_counter ID DIR: wait until the job runs and has verified 3 steps.
start_counter() {
    local id="$1" dir="$2" st
    if ! st="$(wait_state "$id" 120 running)"; then
        bad "job $id did not start: $st"; return 1
    fi
    if ! wait_progress "$dir" 3 60; then
        bad "job $id did not verify 3 steps within 60s (at step $(progress_step_or_zero "$dir"))"
        return 1
    fi
    ok "job $id running, step $(progress_step_or_zero "$dir")"
}

# checkpoint_job ID DIR KIND: checkpoint and check the dumped job. Sets
# dumped_step.
dumped_step=0
checkpoint_job() {
    local id="$1" dir="$2" kind="$3" st size s1 s2 pids before
    before="$(job_gpu_pids | tr '\n' ' ')"
    if [ "$kind" = check ] && [ -z "$before" ] && [ "$(job_state "$id")" = running ]; then
        info "nvidia-smi does not list job $id's process by name; the process check only covers names"
    fi
    local t0=$SECONDS
    if ! gpueue checkpoint "$id" >/dev/null; then
        bad "gpueue checkpoint $id was refused"; return 1
    fi
    if ! st="$(wait_state "$id" "$timeout" checkpointed)"; then
        bad "job $id did not become Checkpointed: $st"; return 1
    fi
    size="$(job_json "$id" | jq -r '.checkpoint_size_bytes // 0')"
    if [ "$size" -gt 0 ]; then ok "job $id Checkpointed in $((SECONDS - t0))s, image $(human "$size")"
    else bad "job $id Checkpointed without a checkpoint size"; return 1; fi
    if [ "$kind" = check ]; then
        if gpu_slot_holds "$id"; then bad "GPU $gpu still lists job $id in a slot"; return 1; fi
        pids="$(job_gpu_pids | tr '\n' ' ')"
        if [ -n "$pids" ]; then bad "nvidia-smi still shows the job's processes: $pids"; return 1; fi
        ok "GPU $gpu slot free, no process of the job in nvidia-smi"
    fi
    s1="$(progress_step_or_zero "$dir")"
    sleep 4
    s2="$(progress_step_or_zero "$dir")"
    if [ "$s1" != "$s2" ]; then bad "progress advanced while Checkpointed ($s1 -> $s2)"; return 1; fi
    ok "progress stands still at step $s1"
    dumped_step="$s1"
}

# restore_job ID DIR EXPECTED(running|paused): restore and check that the
# counter continues from dumped_step.
restore_job() {
    local id="$1" dir="$2" want="$3" st t0=$SECONDS
    if ! gpueue restore "$id" >/dev/null; then
        bad "gpueue restore $id was refused"; return 1
    fi
    if ! st="$(wait_state "$id" "$timeout" "$want")"; then
        if [ "$st" = checkpointed:error ]; then
            local err
            err="$(last_error "$id")"
            bad "restore of job $id FAILED (the job is Checkpointed again): ${err:0:160}$([ "${#err}" -gt 160 ] && echo ...)"
        else
            bad "job $id did not come back $want: $st"
        fi
        return 1
    fi
    if [ "$want" = paused ]; then
        local s1
        s1="$(progress_step_or_zero "$dir")"
        sleep 4
        if [ "$(progress_step_or_zero "$dir")" != "$s1" ]; then
            bad "job $id restored Paused but its progress advanced"; return 1
        fi
        ok "job $id restored Paused in $((SECONDS - t0))s, still at step $s1"
        return 0
    fi
    ok "job $id restored in $((SECONDS - t0))s"
    check_continues "$id" "$dir"
}

check_continues() {
    local id="$1" dir="$2"
    if ! wait_progress "$dir" $((dumped_step + 2)) 60; then
        bad "job $id did not continue past step $dumped_step (at $(progress_step_or_zero "$dir"); state $(job_state "$id"))"
        return 1
    fi
    if ! history_ok "$dir"; then
        bad "job $id's steps are not continuous after the restore:"
        tail -n 8 "$dir/history" | tr '\n' ' ' | sed 's/^/            /'; echo
        return 1
    fi
    ok "job $id continues from step $dumped_step (now $(progress_step_or_zero "$dir")), every step verified once"
}

# finish_job ID DIR: create the stop file and expect Success.
finish_job() {
    local id="$1" dir="$2" st
    touch "$dir/stop"
    if ! st="$(wait_state "$id" 60 done:success)"; then
        bad "job $id did not end with Success: $st"; return 1
    fi
    if ! history_ok "$dir"; then bad "job $id's history is not continuous"; return 1; fi
    ok "job $id stopped: Success after $(progress_step_or_zero "$dir") steps"
}

# --- scenarios --------------------------------------------------------------

scenario_a() {
    local dir="$workdir/a" id
    add_job gpu-cycles check "$dir" || return 1; id="$new_id"
    start_counter "$id" "$dir" || return 1
    for cycle in 1 2 3; do
        info "cycle $cycle/3"
        wait_progress "$dir" $(($(progress_step_or_zero "$dir") + 2)) 30 || { bad "job $id stalled"; return 1; }
        checkpoint_job "$id" "$dir" check || return 1
        restore_job "$id" "$dir" running || return 1
    done
    finish_job "$id" "$dir"
}

scenario_b() {
    local dir="$workdir/b" id st s1
    add_job gpu-paused check "$dir" || return 1; id="$new_id"
    start_counter "$id" "$dir" || return 1
    gpueue pause "$id" >/dev/null || { bad "gpueue pause $id was refused"; return 1; }
    st="$(wait_state "$id" 30 paused)" || { bad "job $id did not pause: $st"; return 1; }
    s1="$(progress_step_or_zero "$dir")"
    sleep 3
    [ "$(progress_step_or_zero "$dir")" = "$s1" ] || { bad "progress advanced while Paused"; return 1; }
    ok "job $id Paused at step $s1"
    checkpoint_job "$id" "$dir" check || return 1
    restore_job "$id" "$dir" paused || return 1
    gpueue start "$id" >/dev/null || { bad "gpueue start $id was refused"; return 1; }
    st="$(wait_state "$id" 30 running)" || { bad "job $id did not resume: $st"; return 1; }
    check_continues "$id" "$dir" || return 1
    finish_job "$id" "$dir"
}

scenario_c() {
    local dir="$workdir/c" id
    add_job cpu-cycles cpu "$dir" || return 1; id="$new_id"
    start_counter "$id" "$dir" || return 1
    for cycle in 1 2; do
        info "cycle $cycle/2"
        wait_progress "$dir" $(($(progress_step_or_zero "$dir") + 2)) 30 || { bad "job $id stalled"; return 1; }
        checkpoint_job "$id" "$dir" cpu || return 1
        restore_job "$id" "$dir" running || return 1
    done
    finish_job "$id" "$dir"
}

scenario_d() {
    local dir="$workdir/d1" dir2="$workdir/d2" id id2 kept st
    add_job gpu-kept check "$dir" || return 1; id="$new_id"
    start_counter "$id" "$dir" || return 1
    checkpoint_job "$id" "$dir" check || return 1
    kept="$dumped_step"
    add_job gpu-between check "$dir2" || return 1; id2="$new_id"
    info "job $id2 runs on GPU $gpu while job $id is Checkpointed"
    start_counter "$id2" "$dir2" || return 1
    finish_job "$id2" "$dir2" || return 1
    st="$(job_state "$id")"
    [ "$st" = checkpointed ] || { bad "job $id is $st instead of Checkpointed"; return 1; }
    [ "$(progress_step_or_zero "$dir")" = "$kept" ] || { bad "job $id's progress changed while Checkpointed"; return 1; }
    ok "job $id still Checkpointed at step $kept"
    dumped_step="$kept"
    restore_job "$id" "$dir" running || return 1
    finish_job "$id" "$dir"
}

scenario_e() {
    local dir="$workdir/e" id
    add_job gpu-nockpt check "$dir" --no-checkpoint || return 1; id="$new_id"
    start_counter "$id" "$dir" || return 1
    finish_job "$id" "$dir"
}

describe() {
    case "$1" in
        a) echo "GPU job, 3 checkpoint/restore cycles" ;;
        b) echo "paused GPU job: pause, checkpoint, restore Paused, start" ;;
        c) echo "CPU job, 2 checkpoint/restore cycles" ;;
        d) echo "GPU job checkpointed while another GPU job runs, then restored" ;;
        e) echo "GPU job without checkpoint (--no-checkpoint)" ;;
    esac
}

# --- failure details and cleanup -------------------------------------------

journal_lines() {
    # The helper and the daemon log through the journal; reading it needs the
    # adm or systemd-journal group.
    journalctl -u gpueue-helper -u gpueue --since "$since" --no-pager -o short-iso 2>/dev/null
}

print_failure_details() {
    local id j
    for id in "${scenario_jobs[@]}"; do
        j="$(job_json "$id")"
        say "    --- job $id: $(job_state "$id")"
        if [ -n "$j" ]; then
            local err
            err="$(jq -r '.details.last_error // empty' <<<"$j")"
            [ -n "$err" ] && say "    last_error: $err" | fold -s -w 110 | sed '2,$s/^/      /'
        fi
        say "    last log lines:"
        gpueue log -l 12 "$id" 2>&1 | sed -n '/^output:/,$p' | sed 's/^/      /'
        local lines
        lines="$(journal_lines | grep -E "job $id([^0-9]|$)")"
        if [ -n "$lines" ]; then
            say "    journal (gpueue, gpueue-helper) about job $id:"
            tail -n 15 <<<"$lines" | sed 's/^/      /'
        elif ! journalctl -n 0 >/dev/null 2>&1; then
            say "    (the journal is not readable; join the adm or systemd-journal group to see it)"
        fi
    done
}

# Kill and remove the given jobs, waiting for dumps and restores to settle.
remove_jobs() {
    local id st
    for id in "$@"; do
        st="$(job_state "$id")"
        case "$st" in
            checkpointing | restoring | restore_queued)
                st="$(wait_state "$id" "$timeout" running paused checkpointed)" ;;
        esac
        case "$st" in
            running | paused)
                gpueue kill "$id" >/dev/null 2>&1
                wait_state "$id" 60 done:killed >/dev/null ;;
        esac
    done
    for id in "$@"; do
        [ "$(job_state "$id")" = gone ] && continue
        gpueue remove "$id" >/dev/null 2>&1 || say "  could not remove job $id ($(job_state "$id"))"
    done
}

cleanup() {
    local rc=$?
    trap - EXIT INT TERM
    if [ -n "$workdir" ] && [ -d "$workdir" ]; then
        # Let the counters exit on their own where they still run.
        for d in "$workdir"/*/; do [ -d "$d" ] && touch "$d/stop"; done
    fi
    if [ "$keep" = 1 ]; then
        say "--keep: leaving jobs ${jobs_created[*]:-(none)} and $workdir"
        exit "$rc"
    fi
    if [ "${#jobs_created[@]}" -gt 0 ]; then
        say "Cleanup: removing this run's jobs ${jobs_created[*]}"
        remove_jobs "${jobs_created[@]}"
        local left
        left="$(gpueue status --json 2>/dev/null |
            jq -r --arg p "$run_id-" '.jobs[] | select((.label // "") | startswith($p)) | "\(.id) \(.state.state)"')"
        [ -z "$left" ] || { say "  jobs of this run still listed:"; say "$left"; rc=1; }
        if [ -n "$(job_gpu_pids)" ]; then
            say "  processes of this run still hold the GPU: $(job_gpu_pids | tr '\n' ' ')"; rc=1
        fi
    fi
    [ -n "$workdir" ] && rm -rf "$workdir"
    exit "$rc"
}
trap cleanup EXIT
trap 'say "interrupted"; exit 130' INT TERM

# --- preflight --------------------------------------------------------------

preflight_failed=0
pf_fail() { bad "$*"; preflight_failed=1; }

say "== Preflight"
for tool in gpueue jq nvidia-smi "$cc"; do
    command -v "$tool" >/dev/null 2>&1 || pf_fail "$tool not found"
done
if [ "$preflight_failed" = 1 ]; then say "PREFLIGHT FAIL"; exit 1; fi

say "    gpueue:  $(gpueue --version 2>&1)"
gpu_line="$(nvidia-smi --query-gpu=index,name,driver_version --format=csv,noheader -i "$gpu" 2>&1)"
say "    GPU $gpu:   ${gpu_line#*, }"
say "    run id:  $run_id"

doctor="$(gpueue doctor --color never 2>&1)"
doctor_rc=$?
if grep -q '^FAIL' <<<"$doctor" || [ "$doctor_rc" -ne 0 ]; then
    pf_fail "gpueue doctor reports problems:"
    grep -E '^(FAIL|WARN)' <<<"$doctor" | sed 's/^/          /'
else
    ok "gpueue doctor: no FAIL"
    grep -E '^WARN' <<<"$doctor" | sed 's/^/          /'
fi

gpus="$(gpueue gpu --json 2>/dev/null)"
if [ -z "$gpus" ]; then
    pf_fail "gpueue gpu --json failed"
elif ! jq -e --argjson g "$gpu" '.gpus[] | select(.index == $g)' >/dev/null <<<"$gpus"; then
    pf_fail "gpueue does not list GPU $gpu"
elif ! jq -e --argjson g "$gpu" '.gpus[] | select(.index == $g) | (.available // true) and .used < .parallel' >/dev/null <<<"$gpus"; then
    pf_fail "GPU $gpu has no free slot: $(jq -c --argjson g "$gpu" '.gpus[] | select(.index == $g) | {used, parallel, jobs}' <<<"$gpus")"
else
    ok "GPU $gpu listed by gpueue with a free slot ($(jq -r --argjson g "$gpu" '.gpus[] | select(.index == $g) | "\(.used)/\(.parallel) used"' <<<"$gpus"))"
fi

busy="$(gpueue status --json 2>/dev/null | jq -r --arg g "$group" \
    '[.jobs[] | select(.group == $g and (.state.state | IN("running","paused","queued","restore_queued","restoring","checkpointing")))] | length')"
[ "${busy:-0}" = 0 ] || info "group $group already has $busy active job(s); this run's jobs may wait for them"

workdir="$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/gpueue-host-test.XXXXXX")" || { pf_fail "cannot create a work directory"; exit 1; }
mkdir -p "$workdir/bin"
if "$cc" -O2 -Wall -o "$workdir/bin/check" "$src/check.c" -ldl 2>"$workdir/cc.log" &&
    "$cc" -O2 -Wall -o "$workdir/bin/cpu" "$src/cpu.c" 2>>"$workdir/cc.log"; then
    ok "built the counters in $workdir/bin with $cc"
else
    pf_fail "building the counters failed:"; sed 's/^/          /' "$workdir/cc.log"
fi
# The GPU counter loads libcuda itself; check that the driver library exists.
if ! { ldconfig -p || /sbin/ldconfig -p; } 2>/dev/null | grep -q 'libcuda\.so\.1'; then
    pf_fail "libcuda.so.1 (the NVIDIA driver's CUDA library) not found"
fi

if [ "$preflight_failed" = 1 ]; then say "PREFLIGHT FAIL"; exit 1; fi
say "PREFLIGHT PASS"

# --- run --------------------------------------------------------------------

failed=0
for s in ${only//,/ }; do
    say ""
    say "== Scenario $s: $(describe "$s")"
    scenario_jobs=()
    fail_reason=""
    start=$SECONDS
    if "scenario_$s"; then
        say "PASS  scenario $s ($((SECONDS - start))s)"
        results+=("PASS  $s  $(describe "$s")")
    else
        say "FAIL  scenario $s ($((SECONDS - start))s)"
        results+=("FAIL  $s  $(describe "$s")" "        ${fail_reason:0:200}")
        failed=1
        print_failure_details
        if [ "$keep" = 0 ] && [ "${#scenario_jobs[@]}" -gt 0 ]; then
            for d in "$workdir/$s"*/; do [ -d "$d" ] && touch "$d/stop"; done
            remove_jobs "${scenario_jobs[@]}"
        fi
    fi
done

say ""
say "== Summary ($(gpueue --version 2>&1), ${gpu_line#*, })"
printf '  %s\n' "${results[@]}"
if [ "$failed" = 0 ]; then say "ALL PASSED"; else say "SOME SCENARIOS FAILED"; fi
exit "$failed"
