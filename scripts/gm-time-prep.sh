#!/bin/sh
# gm-time-prep.sh — gate for the gPTP grandmaster service (ExecStartPre).
#
#   gm-time-prep.sh <router-ip> [internet-host ...]
#
# Waits for the router, then for the internet hosts (default 1.1.1.1 and
# google.com, which also proves DNS), then starts systemd-timesyncd so the
# system clock is set once by NTP, stops it again so nothing steps or slews
# the clock while the grandmaster serves, and writes the time to the RTC
# for the next boot. Always exits 0: a prep failure starts the GM unsynced
# with a WARNING in the journal rather than no GM at all.
#
# Environment: GM_PREP_MAX_WAIT=<s> overall budget (default 600, 0 = forever).

set -u

ROUTER="${1:?usage: gm-time-prep.sh <router-ip> [internet-host ...]}"
shift
[ $# -gt 0 ] || set -- 1.1.1.1 google.com
MAX_WAIT="${GM_PREP_MAX_WAIT:-600}"
TAG=gm-time-prep

log() {
    logger -t "$TAG" "$*" 2>/dev/null
    echo "$TAG: $*"
}
up() { ping -c1 -W2 "$1" >/dev/null 2>&1; }
start=$(date +%s)
expired() {
    [ "$MAX_WAIT" -gt 0 ] || return 1
    [ $(( $(date +%s) - start )) -ge "$MAX_WAIT" ]
}
all_up() {
    for h in "$@"; do up "$h" || return 1; done
}
give_up() {
    systemctl stop systemd-timesyncd.service 2>/dev/null
    log "WARNING: $1 — starting the grandmaster with an unsynced clock"
    exit 0
}

# 1. router
until up "$ROUTER"; do
    expired && give_up "router $ROUTER not reachable within ${MAX_WAIT}s"
    sleep 2
done
log "router $ROUTER reachable"

# 2. internet + DNS
until all_up "$@"; do
    expired && give_up "internet/DNS check ($*) failed within ${MAX_WAIT}s"
    sleep 3
done
log "internet reachable ($*)"

# 3. NTP — started only now, so its first poll happens immediately
systemctl start systemd-timesyncd.service
until [ "$(timedatectl show -p NTPSynchronized --value)" = yes ]; do
    expired && give_up "NTP did not sync within ${MAX_WAIT}s"
    sleep 2
done

# 4. freeze the clock, seed the RTC
systemctl stop systemd-timesyncd.service
if command -v hwclock >/dev/null 2>&1 && hwclock -w 2>/dev/null; then
    rtc="RTC written"
else
    rtc="RTC not written (no hwclock or no RTC)"
fi
log "clock set by NTP at $(date -u +%FT%TZ), timesyncd stopped, $rtc — starting the grandmaster"
exit 0
