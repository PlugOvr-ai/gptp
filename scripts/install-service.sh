#!/usr/bin/env bash
# Install gptp-gm as a systemd service from this source tree (for the
# prebuilt release binary use install.sh, see README.md).
#
#   sudo ./scripts/install-service.sh eth0 --handover 15
#   sudo ./scripts/install-service.sh eth0 --monitor --capture-dir /var/log/gptp/captures
#   sudo ./scripts/install-service.sh --uninstall [gm|monitor]
#
# Builds the release binary (incrementally, as the invoking user), then
# hands it to install.sh, which installs it and sets up the service: the
# interface first, then gptp-gm options. Grandmaster mode installs the
# service gptp-gm, --monitor installs gptp-monitor, so both can coexist (on
# different interfaces). The directory of a --log-metrics file and a
# --capture-dir are made writable for the service.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi
if [[ $(id -u) -ne 0 ]]; then
    echo "error: must run as root (sudo $0 ...)" >&2
    exit 1
fi
if [[ ${1:-} == "--uninstall" ]]; then
    exec sh "$REPO_ROOT/install.sh" "$@"
fi

# Always (re)build: cargo is incremental, and an old binary would lack
# newer options.
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

GPTP_BINARY="$REPO_ROOT/target/release/gptp-gm" exec sh "$REPO_ROOT/install.sh" "$@"
