#!/usr/bin/env bash
# Install (or remove) gptp-gm as a systemd service.
#
#   sudo ./scripts/install-service.sh eth0 --handover 15
#   sudo ./scripts/install-service.sh eth0 --monitor --log-metrics /var/log/gptp/monitor.jsonl
#   sudo ./scripts/install-service.sh --uninstall [gm|monitor]
#
# The interface is required; everything after it is passed verbatim to
# gptp-gm (e.g. --handover 15 --priority1 1). --no-tui is added automatically.
# Grandmaster mode installs the service gptp-gm, --monitor installs
# gptp-monitor, so both can coexist (on different interfaces). The
# directory of a --log-metrics file is made writable for the service.
# --uninstall without an argument removes both services and the binary.

set -euo pipefail

BIN_DST=/usr/local/bin/gptp-gm
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
ALL_SERVICES=(gptp-gm gptp-monitor)

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

unit_path() {
    echo "/etc/systemd/system/$1.service"
}

[[ ${1:-} == "-h" || ${1:-} == "--help" ]] && usage 0

if [[ $(id -u) -ne 0 ]]; then
    echo "error: must run as root (sudo $0 ...)" >&2
    exit 1
fi

if [[ ${1:-} == "--uninstall" ]]; then
    case ${2:-all} in
        gm) remove=(gptp-gm) ;;
        monitor) remove=(gptp-monitor) ;;
        all) remove=("${ALL_SERVICES[@]}") ;;
        *) echo "error: --uninstall takes gm, monitor, or nothing" >&2; usage 1 ;;
    esac
    for svc in "${remove[@]}"; do
        if [[ -e $(unit_path "$svc") ]]; then
            systemctl disable --now "$svc" 2>/dev/null || true
            rm -f "$(unit_path "$svc")"
            echo "removed $svc service"
        fi
    done
    systemctl daemon-reload
    # Keep the binary while another service still uses it.
    remaining=0
    for svc in "${ALL_SERVICES[@]}"; do
        [[ -e $(unit_path "$svc") ]] && remaining=1
    done
    if [[ $remaining -eq 0 ]]; then
        rm -f "$BIN_DST"
        echo "removed $BIN_DST"
    else
        echo "kept $BIN_DST (still used by an installed service)"
    fi
    exit 0
fi

IFACE=${1:-}
if [[ -z $IFACE || $IFACE == -* ]]; then
    echo "error: first argument must be the network interface" >&2
    usage 1
fi
shift
EXTRA_ARGS=("$@")

MONITOR=0
METRICS_FILE=
for ((i = 0; i < ${#EXTRA_ARGS[@]}; i++)); do
    case ${EXTRA_ARGS[i]} in
        --monitor) MONITOR=1 ;;
        --log-metrics) METRICS_FILE=${EXTRA_ARGS[i + 1]:-} ;;
        --log-metrics=*) METRICS_FILE=${EXTRA_ARGS[i]#--log-metrics=} ;;
    esac
done

if [[ $MONITOR -eq 1 ]]; then
    SERVICE=gptp-monitor
    OTHER=gptp-gm
    DESCRIPTION="gPTP slave monitor on ${IFACE}"
else
    SERVICE=gptp-gm
    OTHER=gptp-monitor
    DESCRIPTION="gPTP grandmaster on ${IFACE}"
fi
UNIT=$(unit_path "$SERVICE")

if [[ ! -e /sys/class/net/$IFACE ]]; then
    echo "warning: interface $IFACE does not exist right now — installing anyway" >&2
fi
if grep -qs "^ExecStart=$BIN_DST $IFACE " "$(unit_path "$OTHER")"; then
    echo "warning: $OTHER is also installed on $IFACE — both would answer peer" \
        "delay on the same port; remove one with: sudo $0 --uninstall" \
        "$([[ $OTHER == gptp-gm ]] && echo gm || echo monitor)" >&2
fi

if [[ -n $METRICS_FILE && $METRICS_FILE != /* ]]; then
    echo "error: --log-metrics needs an absolute path for the service" >&2
    exit 1
fi

# Always (re)build: cargo is incremental, and an old binary would lack
# newer options such as --monitor.
BIN_SRC=$REPO_ROOT/target/release/gptp-gm
echo "building release binary..."
if [[ -n ${SUDO_USER:-} ]]; then
    # Build as the invoking user (login shell, so ~/.cargo/bin is on PATH)
    # so target/ does not become root-owned.
    build=(sudo -u "$SUDO_USER" -i -- cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml")
else
    build=(cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml")
fi
if ! "${build[@]}"; then
    echo "warning: build failed — installing the existing binary if there is one" >&2
fi
if [[ ! -x $BIN_SRC ]]; then
    echo "error: no binary at $BIN_SRC and cargo is unavailable" >&2
    exit 1
fi
if [[ $MONITOR -eq 1 ]] && ! "$BIN_SRC" --help | grep -q -- --monitor; then
    echo "error: $BIN_SRC does not support --monitor — rebuild it" >&2
    exit 1
fi

install -m 755 "$BIN_SRC" "$BIN_DST"

RW_PATHS=
if [[ -n $METRICS_FILE ]]; then
    METRICS_DIR=$(dirname "$METRICS_FILE")
    mkdir -p "$METRICS_DIR"
    RW_PATHS="ReadWritePaths=${METRICS_DIR}"
fi

cat > "$UNIT" <<EOF
[Unit]
Description=${DESCRIPTION}
Documentation=https://github.com/PlugOvr-ai/gptp
After=network-online.target sys-subsystem-net-devices-${IFACE}.device
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_DST} ${IFACE} --no-tui${EXTRA_ARGS[@]:+ ${EXTRA_ARGS[*]}}
# Runs as root for /dev/ptp* (PTP hardware clock) access, with capabilities
# bounded to what raw sockets + NIC timestamping configuration need.
CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
${RW_PATHS}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable "$SERVICE"
# restart, not start: a reinstall must pick up the new binary and arguments.
systemctl restart "$SERVICE"

echo
systemctl --no-pager --lines 0 status "$SERVICE" || true
echo
echo "installed: $UNIT"
echo "logs:      journalctl -fu $SERVICE"
[[ -n $METRICS_FILE ]] && echo "metrics:   $METRICS_FILE"
echo "remove:    sudo $0 --uninstall $([[ $MONITOR -eq 1 ]] && echo monitor || echo gm)"
