#!/bin/bash
# Runs inside the privileged measurement container (see
# scripts/measure-checkpoint.sh). For each "<vram_gb>:<ram_gb>" case it
# starts scripts/measure/job.py, then twice: lock, checkpoint, criu dump,
# criu restore, CUDA restore, unlock - the same commands the helper runs in
# manual mode - once restoring from a warm page cache and once after
# evicting the image from the page cache. Prints one CSV line per cycle.
#
# MODE=plugin instead lets CRIU's CUDA plugin do the CUDA part: the lock and
# checkpoint are then inside the criu dump time and the CUDA restore and
# unlock inside the criu restore time (their own columns print "-").
set -euo pipefail
DATA=${DATA:-/data}
MODE=${MODE:-manual}
PY=/opt/venv/bin/python3
CRIU_COMMON=(-v4 --tcp-close)
if [ "$MODE" = manual ]; then
    EMPTY=$DATA/no-plugins
    mkdir -p "$EMPTY"
    CRIU_COMMON+=(-L "$EMPTY")
fi
cuda_manual() { [ "$vram" != 0 ] && [ "$MODE" = manual ]; }

now() { date +%s.%N; }
dt() { python3 -c "print(f'{$2-$1:.2f}')"; }

# evict <dir>: write back and drop the image files from the page cache.
evict() {
    sync
    "$PY" - "$1" <<'EOF'
import os, sys
for root, _, files in os.walk(sys.argv[1]):
    for f in files:
        fd = os.open(os.path.join(root, f), os.O_RDONLY)
        os.fsync(fd)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
EOF
}
cached_pct() { # share of the image's pages in the page cache (mincore)
    "$PY" - "$1" <<'EOF2'
import ctypes, mmap, os, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]
page = os.sysconf("SC_PAGE_SIZE")
res = tot = 0
for root, _, files in os.walk(sys.argv[1]):
    for f in files:
        path = os.path.join(root, f)
        size = os.path.getsize(path)
        if size == 0:
            continue
        fd = os.open(path, os.O_RDONLY)
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        n = (size + page - 1) // page
        vec = ctypes.create_string_buffer(n)
        libc.mincore(ctypes.c_void_p(addr), size, vec)
        res += sum(b & 1 for b in vec.raw)
        tot += n
        libc.munmap(ctypes.c_void_p(addr), size)
        os.close(fd)
print(f"{100 * res / tot:.0f}" if tot else "?")
EOF2
}
last_step() { grep -c '^step ' "$1" || true; }

echo "mode,case,cycle,cache,rss_gb,vram_gb,lock_s,cuda_ckpt_s,criu_dump_s,image_gb,criu_restore_s,cuda_restore_s,unlock_s,cached_pct_before_restore,ok"
for c in "$@"; do
    vram=${c%%:*}
    ram=${c##*:}
    run=$DATA/run-$vram-$ram
    rm -rf "$run"
    mkdir -p "$run"
    setsid "$PY" -u /m/job.py "$vram" "$ram" >"$run/job.log" 2>&1 </dev/null &
    pid=$!
    for _ in $(seq 600); do grep -q '^step 3 ' "$run/job.log" && break; sleep 0.2; done
    grep -q '^step 3 ' "$run/job.log" || { cat "$run/job.log"; exit 1; }
    for cycle in $(seq "${CYCLES:-2}"); do
        rss=$(awk '/VmRSS/ {printf "%.2f", $2/1048576}' /proc/$pid/status)
        used=0
        if [ "$vram" != 0 ]; then
            used=$(nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits \
                | awk -F', ' -v p="$pid" '$1 == p {printf "%.2f", $2/1024}')
            used=${used:-?}
        fi
        img=$run/img-$cycle
        mkdir -p "$img"
        lock=- ck=- res=- unl=-
        if cuda_manual; then
            t0=$(now); cuda-checkpoint --action lock --pid "$pid" --timeout 30000; t1=$(now)
            cuda-checkpoint --action checkpoint --pid "$pid"; t2=$(now)
            lock=$(dt "$t0" "$t1"); ck=$(dt "$t1" "$t2")
        fi
        t2=$(now)
        criu dump -t "$pid" -D "$img" -o "$run/dump-$cycle.log" "${CRIU_COMMON[@]}" --link-remap
        t3=$(now)
        dump=$(dt "$t2" "$t3")
        size=$(du -sb "$img" | awk '{printf "%.2f", $1/1073741824}')
        if [ $((cycle % 2)) = 1 ]; then cache=warm; else cache=cold; evict "$img"; fi
        cached=$(cached_pct "$img")
        before=$(last_step "$run/job.log")
        t4=$(now)
        criu restore -D "$img" -o "$run/restore-$cycle.log" "${CRIU_COMMON[@]}" \
            --restore-detached --pidfile "$run/pid-$cycle"
        t5=$(now)
        rst=$(dt "$t4" "$t5")
        pid=$(cat "$run/pid-$cycle")
        if cuda_manual; then
            cuda-checkpoint --action restore --pid "$pid"; t6=$(now)
            cuda-checkpoint --action unlock --pid "$pid"; t7=$(now)
            res=$(dt "$t5" "$t6"); unl=$(dt "$t6" "$t7")
        fi
        # The job must go on with intact memory.
        for _ in $(seq 100); do [ "$(last_step "$run/job.log")" -ge $((before + 3)) ] && break; sleep 0.2; done
        ok=bad
        if [ "$(last_step "$run/job.log")" -ge $((before + 3)) ] && ! grep -q ' ok 0$' "$run/job.log"; then ok=ok; fi
        echo "$MODE,$vram:$ram,$cycle,$cache,$rss,$used,$lock,$ck,$dump,$size,$rst,$res,$unl,$cached,$ok"
        rm -rf "$img"
    done
    kill -9 "$pid" 2>/dev/null || true
    sleep 1
    rm -rf "$run"
done
