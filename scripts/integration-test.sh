#!/usr/bin/env bash
# End-to-end tests in a throwaway network namespace — no hardware needed.
#
#   scripts/integration-test.sh [path/to/gptp-gm]    (default: target/release/gptp-gm)
#
# Runs as root (CI: sudo) or unprivileged inside a user namespace. Uses
# veth pairs and a Linux bridge, so only software timestamping is exercised.
#
# 1. Handover + monitor: GM A hands over to GM C, which runs 3 s off; the
#    monitor must report Sync loss, the GM change, and a +3 s time step.
# 2. Jitter burst (needs the netem qdisc, skipped if unavailable): 2 ms ±
#    1 ms delay toward the monitor for 4 s; the monitor must diagnose a
#    jitter burst caused by path delay and write a capture file.

set -euo pipefail

BIN=$(realpath "${1:-target/release/gptp-gm}")
if [[ ! -x $BIN ]]; then
    echo "error: $BIN not found — run cargo build --release first" >&2
    exit 2
fi

# Re-run inside a fresh network namespace.
if [[ -z ${GPTP_IN_NETNS:-} ]]; then
    if [[ $(id -u) -eq 0 ]]; then
        exec unshare -n env GPTP_IN_NETNS=1 "$0" "$BIN"
    else
        exec unshare -rn env GPTP_IN_NETNS=1 "$0" "$BIN"
    fi
fi

OUT=$(mktemp -d)
FAILED=0

stop_all() {
    local pids
    mapfile -t pids < <(jobs -p)
    [[ ${#pids[@]} -gt 0 ]] && kill -INT "${pids[@]}" 2>/dev/null
    wait 2>/dev/null || true
}
trap stop_all EXIT

check() { # <file> <extended regex> <description>
    if grep -qE -- "$2" "$1"; then
        echo "  ok    $3"
    else
        echo "  FAIL  $3  (no match for /$2/ in $(basename "$1"))"
        FAILED=1
    fi
}

dump_on_failure() {
    if [[ $FAILED -ne 0 ]]; then
        for f in "$@"; do
            echo "----- $(basename "$f") (events) -----"
            grep -vE '^(status|node|sync):' "$f" || true
        done
    fi
}

# Bridge with one veth per named endpoint; forward the gPTP multicast
# (01:80:C2:00:00:0E), which Linux bridges drop by default.
setup_bridge() {
    ip link add br0 type bridge group_fwd_mask 0x4000
    for x in "$@"; do
        ip link add "veth$x" type veth peer name "p$x"
        ip link set "p$x" master br0
        ip link set "p$x" up
        ip link set "veth$x" up
    done
    ip link set br0 up
    # Frames sent while the fresh ports come up get lost.
    sleep 2
}

teardown_bridge() {
    for x in "$@"; do
        ip link del "veth$x" 2>/dev/null || true
    done
    ip link del br0 2>/dev/null || true
}

run() { # <log> <args...>
    local log=$1
    shift
    "$BIN" "$@" --no-tui --no-inventory >"$log" 2>&1 &
}

ip link set lo up

echo "== 1. GM handover with time step, observed by the monitor"
setup_bridge A C M
run "$OUT/a.log" vethA --handover 0.1
run "$OUT/c.log" vethC --priority1 200 --utc-offset 40
run "$OUT/m.log" vethM --monitor --capture-dir "$OUT/cap1"
sleep 22
stop_all
check "$OUT/a.log" 'Handover: stopped Announce/Sync' "GM A hands over"
check "$OUT/c.log" 'Foreign master seen' "GM C sees GM A"
check "$OUT/m.log" 'Following master port .* \(prio1 1,' "monitor follows GM A"
check "$OUT/m.log" 'ALARM: Sync stopped' "monitor detects Sync loss"
check "$OUT/m.log" 'ALARM: grandmaster changed' "monitor detects GM change"
check "$OUT/m.log" 'Sync resumed after' "monitor sees Sync resume"
check "$OUT/m.log" 'ALARM: master time STEP \+(2\.99|3\.00)[0-9]* s' "monitor measures the +3 s step"
check "$OUT/m.log" 'Captured [0-9]+ Syncs around \[.*time step' "monitor writes a capture"
dump_on_failure "$OUT/a.log" "$OUT/c.log" "$OUT/m.log"
teardown_bridge A C M

echo "== 2. Path-delay jitter burst, diagnosed by the monitor"
setup_bridge A M
if tc qdisc add dev pM root netem delay 1ms 2>/dev/null; then
    tc qdisc del dev pM root
    run "$OUT/a2.log" vethA
    run "$OUT/m2.log" vethM --monitor --capture-dir "$OUT/cap2"
    sleep 14
    tc qdisc add dev pM root netem delay 2ms 1ms
    sleep 4
    tc qdisc del dev pM root
    sleep 15
    stop_all
    check "$OUT/m2.log" 'jitter burst — .*Sync arrival jitter' "burst attributed to Sync arrival jitter"
    check "$OUT/m2.log" 'Jitter burst over after' "burst end reported"
    check "$OUT/m2.log" 'Captured [0-9]+ Syncs around \[.*jitter burst' "burst capture written"
    captures=("$OUT"/cap2/sync-*.csv)
    if [[ -f ${captures[0]} ]]; then
        check "${captures[0]}" '^unix_s,seq,error_ns,' "capture has the CSV header"
    fi
    dump_on_failure "$OUT/a2.log" "$OUT/m2.log"
else
    echo "  skip  netem qdisc not available in this kernel"
fi
teardown_bridge A M

if [[ $FAILED -ne 0 ]]; then
    echo "integration tests FAILED (logs in $OUT)"
    exit 1
fi
rm -rf "$OUT"
echo "integration tests passed"
