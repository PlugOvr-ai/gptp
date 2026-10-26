#!/usr/bin/env bash
# Scripted fault scenarios in the Docker gPTP lab, with pass/fail checks.
#
#   lab/run.sh            build, run all scenarios, check, tear down
#   lab/run.sh --keep     leave the lab running afterwards
#
# Scenarios (the monitor sits behind the switch, on the sensor side):
#   1. path-delay jitter: netem 2 ms ± 1 ms for 4 s on the switch port
#      toward the sensors (skipped if the host kernel lacks netem)
#   2. grandmaster failure: gm stops, gm-backup (1 s off) takes over
#   3. grandmaster returns
# Throughout, the GM also serves IEEE 1588 over UDP to ptp-device (ptp4l).
# Logs, metrics and captures are collected in lab/out/<UTC time>/.

set -euo pipefail
cd "$(dirname "$0")"

DC=(docker compose -f compose.yaml)
KEEP=0
[[ ${1:-} == "--keep" ]] && KEEP=1
OUT=out/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT"
FAILED=0

collect() {
    "${DC[@]}" logs --no-color --no-log-prefix monitor >"$OUT/monitor.log" 2>&1 || true
    for svc in gm gm-backup switch ptp-device; do
        "${DC[@]}" logs --no-color --no-log-prefix "$svc" >"$OUT/$svc.log" 2>&1 || true
    done
    "${DC[@]}" cp monitor:/out/. "$OUT/" >/dev/null 2>&1 || true
}

cleanup() {
    if [[ $KEEP -eq 0 ]]; then
        "${DC[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

check() { # <extended regex> <description> [log, default monitor]
    if grep -qE -- "$1" "$OUT/${3:-monitor}.log"; then
        echo "  ok    $2"
    else
        echo "  FAIL  $2  (no match for /$1/)"
        FAILED=1
    fi
}

step() {
    echo "== $*"
}

step "starting lab (build may take a few minutes the first time)"
"${DC[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
"${DC[@]}" up -d --build --wait gm gm-backup monitor ptp-device
echo "   monitor learning the master for 20 s"
sleep 20

PORT=$("${DC[@]}" exec -T switch cat /run/lab/port-2)
step "1. path-delay jitter on switch port $PORT (toward the sensors)"
NETEM=0
if "${DC[@]}" exec -T switch tc qdisc add dev "$PORT" root netem delay 2ms 1ms; then
    NETEM=1
    sleep 4
    "${DC[@]}" exec -T switch tc qdisc del dev "$PORT" root
    sleep 15
else
    echo "   skipped: netem not available in the host kernel (modprobe sch_netem)"
fi

step "2. grandmaster failure"
"${DC[@]}" stop gm >/dev/null 2>&1
sleep 15

step "3. grandmaster returns"
"${DC[@]}" start gm >/dev/null 2>&1
sleep 15

# Stopping the monitor flushes a capture that is still pending.
"${DC[@]}" stop monitor >/dev/null 2>&1
collect

step "checks"
if [[ $NETEM -eq 1 ]]; then
    check 'jitter burst — .*Sync arrival jitter' "jitter burst attributed to path delay"
    check 'Jitter burst over after' "jitter burst end reported"
fi
check 'ALARM: Sync stopped' "Sync loss when the GM fails"
check 'ALARM: grandmaster changed' "GM change detected"
check 'ALARM: master time STEP \+(99[89]\.[0-9]+ ms|1\.00[0-9]* s|1000\.[0-9]+ ms)' "+1 s step on failover to gm-backup"
check 'ALARM: master time STEP -(99[89]\.[0-9]+ ms|1\.00[0-9]* s|1000\.[0-9]+ ms)' "-1 s step when the GM returns"
if grep -E 'STEP .*during a jitter burst' "$OUT/monitor.log" | grep -qE 'STEP [+-](99[89]|1000)'; then
    echo "  FAIL  a GM time step was mislabelled as jitter"
    FAILED=1
fi
check 'ptp1588: UDP.* delay_resp_tx=[1-9]' "GM serves IEEE 1588 beside gPTP (Delay_Req answered)" gm
check 'UNCALIBRATED on RS_SLAVE' "1588 device (ptp4l) follows the GM" ptp-device
check 'rms +[0-9]+ max' "1588 device measures its offset to the GM" ptp-device
captures=("$OUT"/captures/sync-*.csv)
if [[ -f ${captures[0]} ]]; then
    echo "  ok    ${#captures[@]} capture file(s) written"
else
    echo "  FAIL  no capture files"
    FAILED=1
fi

echo
echo "monitor events:"
grep -vE '^(status|node|sync):' "$OUT/monitor.log" | sed 's/^/  /'
echo
echo "results in lab/$OUT"
if [[ $FAILED -ne 0 ]]; then
    echo "lab checks FAILED"
    exit 1
fi
echo "lab checks passed"
