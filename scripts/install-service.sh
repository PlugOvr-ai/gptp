#!/usr/bin/env bash
# Install (or remove) gptp-gm as a systemd service.
#
#   sudo ./scripts/install-service.sh eth0 --handover 15
#   sudo ./scripts/install-service.sh --uninstall
#
# The interface is required; everything after it is passed verbatim to
# gptp-gm (e.g. --handover 15 --priority1 1). --no-tui is added automatically.

set -euo pipefail

SERVICE=gptp-gm
BIN_DST=/usr/local/bin/gptp-gm
UNIT=/etc/systemd/system/${SERVICE}.service
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

usage() {
    sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

[[ ${1:-} == "-h" || ${1:-} == "--help" ]] && usage 0

if [[ $(id -u) -ne 0 ]]; then
    echo "error: must run as root (sudo $0 ...)" >&2
    exit 1
fi

if [[ ${1:-} == "--uninstall" ]]; then
    systemctl disable --now "$SERVICE" 2>/dev/null || true
    rm -f "$UNIT" "$BIN_DST"
    systemctl daemon-reload
    echo "removed $SERVICE service and $BIN_DST"
    exit 0
fi

IFACE=${1:-}
if [[ -z $IFACE || $IFACE == -* ]]; then
    echo "error: first argument must be the network interface" >&2
    usage 1
fi
shift
EXTRA_ARGS=("$@")

if [[ ! -e /sys/class/net/$IFACE ]]; then
    echo "warning: interface $IFACE does not exist right now — installing anyway" >&2
fi

BIN_SRC=$REPO_ROOT/target/release/gptp-gm
if [[ ! -x $BIN_SRC ]]; then
    echo "building release binary..."
    if [[ -n ${SUDO_USER:-} ]]; then
        # Build as the invoking user so target/ does not become root-owned.
        sudo -u "$SUDO_USER" cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    else
        cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml"
    fi
fi

install -m 755 "$BIN_SRC" "$BIN_DST"

cat > "$UNIT" <<EOF
[Unit]
Description=gPTP grandmaster on ${IFACE}
Documentation=https://github.com/PlugOvr-ai/gptp
After=network-online.target sys-subsystem-net-devices-${IFACE}.device
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_DST} ${IFACE} --no-tui${EXTRA_ARGS[@]:+ ${EXTRA_ARGS[*]}}
DynamicUser=yes
AmbientCapabilities=CAP_NET_RAW
CapabilityBoundingSet=CAP_NET_RAW
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now "$SERVICE"

echo
systemctl --no-pager --lines 0 status "$SERVICE" || true
echo
echo "installed: $UNIT"
echo "logs:      journalctl -fu $SERVICE"
echo "remove:    sudo $0 --uninstall"
