#!/bin/sh
# gptp-gm installer.
#
#   curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh
#
# Install and set up the systemd service in one step — the network interface
# first, then gptp-gm options (--no-tui is added automatically):
#
#   curl -fsSL .../install.sh | sudo sh -s -- eth0 --handover 15        # grandmaster: gptp-gm.service
#   curl -fsSL .../install.sh | sudo sh -s -- eth0 --monitor \
#        --log-metrics /var/log/gptp/monitor.jsonl --capture-dir /var/log/gptp/captures
#                                                                        # slave monitor: gptp-monitor.service
#   curl -fsSL .../install.sh | sudo sh -s -- --uninstall [gm|monitor]   # remove service(s); binary when unused
#
# Slave monitor that also keeps this machine's time in sync with the master:
#
#   ... | sudo sh -s -- eth0 --monitor --time-sync builtin [--sync-phc eth1]
#        gptp-gm disciplines the NIC hardware clock, the system clock and
#        further NIC clocks itself (replaces ptp4l + phc2sys)
#   ... | sudo sh -s -- mgbe0_0 --monitor --time-sync linuxptp [--sync-phc mgbe3_0]
#        sets up ptp4l + phc2sys (needs linuxptp installed); gptp-gm
#        monitors listen-only beside it
#
# Both disable conflicting time services (systemd-timesyncd, chrony, ntpd,
# other ptp4l/phc2sys); --uninstall restores them.
#
# Running it again without arguments updates the binary and restarts the
# installed services.
#
# Environment: GPTP_VERSION=v0.2.0 to pin a release, GPTP_BIN_DIR to change
# /usr/local/bin, GPTP_DOWNLOAD_URL to download the release files from a
# mirror instead of GitHub, GPTP_BINARY=<file> to install a local binary
# instead of downloading (used by scripts/install-service.sh).
set -eu

REPO="PlugOvr-ai/gptp"
VERSION="${GPTP_VERSION:-latest}"
BIN_DIR="${GPTP_BIN_DIR:-/usr/local/bin}"
BIN="$BIN_DIR/gptp-gm"
UNIT_DIR=/etc/systemd/system

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

[ "$(uname -s)" = "Linux" ] || die "gptp-gm supports Linux only (found $(uname -s))."

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "run as root: gptp-gm needs raw sockets and runs as a system service"
    SUDO="sudo"
fi

have_systemd() { [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; }
unit_path() { echo "$UNIT_DIR/$1.service"; }

# --- Uninstall -------------------------------------------------------------------------------
if [ "${1:-}" = "--uninstall" ]; then
    case "${2:-all}" in
        gm) remove="gptp-gm" ;;
        monitor) remove="gptp-monitor" ;;
        all) remove="gptp-gm gptp-monitor" ;;
        *) die "--uninstall takes gm, monitor, or nothing" ;;
    esac
    case " $remove " in
        *" gptp-monitor "*)
            if [ -e /etc/gptp-gm/time-sync.state ] && [ -x "$BIN" ]; then
                $SUDO "$BIN" --remove-time-sync || say "warning: undoing the time sync setup failed"
            fi
            ;;
    esac
    for svc in $remove; do
        if [ -e "$(unit_path "$svc")" ]; then
            $SUDO systemctl disable --now "$svc" 2>/dev/null || true
            $SUDO rm -f "$(unit_path "$svc")"
            say "Removed $svc service"
        fi
    done
    if have_systemd; then $SUDO systemctl daemon-reload; fi
    if [ -e "$(unit_path gptp-gm)" ] || [ -e "$(unit_path gptp-monitor)" ]; then
        say "Kept $BIN (still used by an installed service)"
    else
        $SUDO rm -f "$BIN"
        say "Removed $BIN"
    fi
    exit 0
fi

# --- Arguments for the service (validated before downloading anything) ----------------------
IFACE=""
if [ $# -gt 0 ]; then
    IFACE="$1"
    shift
    case "$IFACE" in -*) die "the first argument must be the network interface (e.g. eth0), got $IFACE" ;; esac
    have_systemd || die "no systemd here: install without arguments and run gptp-gm $IFACE ... yourself"
fi
MONITOR=0
METRICS_FILE=""
CAPTURE_DIR=""
TIME_SYNC=""
SYNC_PHCS=""
SETUP_EXTRA=""
prev=""
for arg in "$@"; do
    case "$prev" in
        --log-metrics) METRICS_FILE="$arg" ;;
        --capture-dir) CAPTURE_DIR="$arg" ;;
        --time-sync) TIME_SYNC="$arg" ;;
        --sync-phc) SYNC_PHCS="$SYNC_PHCS $arg" ;;
        --utc-offset | --neighbor-prop-delay-thresh) SETUP_EXTRA="$SETUP_EXTRA $prev $arg" ;;
    esac
    case "$arg" in
        --monitor) MONITOR=1 ;;
        --log-metrics=*) METRICS_FILE="${arg#--log-metrics=}" ;;
        --capture-dir=*) CAPTURE_DIR="${arg#--capture-dir=}" ;;
        --time-sync=*) TIME_SYNC="${arg#--time-sync=}" ;;
        --sync-phc=*) SYNC_PHCS="$SYNC_PHCS ${arg#--sync-phc=}" ;;
        --utc-offset=* | --neighbor-prop-delay-thresh=*) SETUP_EXTRA="$SETUP_EXTRA $arg" ;;
        *" "*) die "arguments with spaces are not supported in the service command line: $arg" ;;
    esac
    prev="$arg"
done
case "$TIME_SYNC" in
    "") ;;
    servo) TIME_SYNC=builtin ;;
    builtin | linuxptp) ;;
    *) die "--time-sync takes builtin or linuxptp, got $TIME_SYNC" ;;
esac
[ -z "$TIME_SYNC" ] || [ "$MONITOR" -eq 1 ] || die "--time-sync needs --monitor"
# The service command line: phc2sys does the --sync-phc work with linuxptp.
SERVICE_ARGS=""
skip=0
for arg in "$@"; do
    if [ "$skip" -eq 1 ]; then
        skip=0
        continue
    fi
    if [ "$TIME_SYNC" = linuxptp ]; then
        case "$arg" in
            --sync-phc) skip=1 && continue ;;
            --sync-phc=*) continue ;;
        esac
    fi
    SERVICE_ARGS="$SERVICE_ARGS $arg"
done
for path in "$METRICS_FILE" "$CAPTURE_DIR"; do
    case "$path" in
        "" | /*) ;;
        *) die "--log-metrics / --capture-dir need absolute paths for the service: $path" ;;
    esac
done

# --- Proxy -----------------------------------------------------------------------------------
# `curl ... | sudo sh` loses the caller's environment (sudo resets it), so also look where
# proxies are configured system-wide. Only the downloads below use it.
PROXY="" PROXY_FROM="" NOPROXY=""
proxy_in_file() { # $1 = variable name pattern, $2... = files
    pattern="$1"
    shift
    sed -n "s/^[[:space:]]*\(export[[:space:]][[:space:]]*\)\{0,1\}\($pattern\)=[\"']\{0,1\}\([^\"'[:space:]]*\).*/\3/p" "$@" 2>/dev/null | head -n 1
}
for v in https_proxy HTTPS_PROXY http_proxy HTTP_PROXY all_proxy ALL_PROXY; do
    eval "val=\${$v:-}"
    if [ -n "$val" ]; then
        PROXY="$val"
        PROXY_FROM="environment"
        break
    fi
done
if [ -z "$PROXY" ]; then
    for f in /etc/environment /etc/profile.d/*.sh; do
        [ -r "$f" ] || continue
        val="$(proxy_in_file 'https_proxy\|HTTPS_PROXY\|http_proxy\|HTTP_PROXY' "$f")"
        if [ -n "$val" ]; then
            PROXY="$val"
            PROXY_FROM="$f"
            break
        fi
    done
fi
if [ -z "$PROXY" ] && command -v apt-config >/dev/null 2>&1; then
    val="$(apt-config dump 2>/dev/null | sed -n 's/^Acquire::https\{0,1\}::Proxy "\(.*\)";$/\1/p' | grep -vi '^\(direct\|false\|\)$' | head -n 1)"
    [ -z "$val" ] || {
        PROXY="$val"
        PROXY_FROM="apt configuration"
    }
fi
if [ -z "$PROXY" ]; then
    for f in /etc/dnf/dnf.conf /etc/yum.conf; do
        val="$(sed -n 's/^proxy[[:space:]]*=[[:space:]]*//p' "$f" 2>/dev/null | head -n 1)"
        if [ -n "$val" ] && [ "$val" != "_none_" ]; then
            PROXY="$val"
            PROXY_FROM="$f"
            break
        fi
    done
fi
if [ -z "$PROXY" ] && [ -n "${SUDO_USER:-}" ] && command -v gsettings >/dev/null 2>&1; then
    # Desktop proxy setting of the user who ran sudo (GNOME and friends).
    if [ "$(sudo -u "$SUDO_USER" gsettings get org.gnome.system.proxy mode 2>/dev/null)" = "'manual'" ]; then
        host="$(sudo -u "$SUDO_USER" gsettings get org.gnome.system.proxy.https host 2>/dev/null | tr -d "'")"
        port="$(sudo -u "$SUDO_USER" gsettings get org.gnome.system.proxy.https port 2>/dev/null)"
        if [ -z "$host" ]; then
            host="$(sudo -u "$SUDO_USER" gsettings get org.gnome.system.proxy.http host 2>/dev/null | tr -d "'")"
            port="$(sudo -u "$SUDO_USER" gsettings get org.gnome.system.proxy.http port 2>/dev/null)"
        fi
        [ -z "$host" ] || [ "${port:-0}" = 0 ] || {
            PROXY="http://$host:$port"
            PROXY_FROM="desktop settings of $SUDO_USER"
        }
    fi
fi
if [ -n "$PROXY" ] && [ -z "${GPTP_BINARY:-}" ]; then
    case "$PROXY" in *://*) ;; *) PROXY="http://$PROXY" ;; esac
    NOPROXY="${no_proxy:-${NO_PROXY:-}}"
    [ -n "$NOPROXY" ] || NOPROXY="$(proxy_in_file 'no_proxy\|NO_PROXY' /etc/environment /etc/profile.d/*.sh)"
    NOPROXY="${NOPROXY:+$NOPROXY,}localhost,127.0.0.1,::1"
    export http_proxy="$PROXY" https_proxy="$PROXY" HTTP_PROXY="$PROXY" HTTPS_PROXY="$PROXY"
    export no_proxy="$NOPROXY" NO_PROXY="$NOPROXY"
    say "Using proxy $(printf '%s' "$PROXY" | sed 's#//[^@/]*@#//***@#') (from $PROXY_FROM)"
fi

# --- Binary ----------------------------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

if [ -n "${GPTP_BINARY:-}" ]; then
    [ -x "$GPTP_BINARY" ] || die "GPTP_BINARY=$GPTP_BINARY is not an executable file"
    cp "$GPTP_BINARY" "$TMP/gptp-gm"
else
    case "$(uname -m)" in
        x86_64 | amd64) TARGET="x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) TARGET="aarch64-unknown-linux-musl" ;;
        armv7* | armv8l) TARGET="armv7-unknown-linux-musleabihf" ;;
        armv6*) TARGET="arm-unknown-linux-musleabihf" ;;
        *) die "no prebuilt binary for CPU architecture $(uname -m) - build from source: git clone https://github.com/$REPO && sudo gptp/scripts/install-service.sh" ;;
    esac

    if [ -n "${GPTP_DOWNLOAD_URL:-}" ]; then
        BASE="${GPTP_DOWNLOAD_URL%/}"
    elif [ "$VERSION" = "latest" ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        BASE="https://github.com/$REPO/releases/download/$VERSION"
    fi

    fetch() {
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL "$1" -o "$2"
        elif command -v wget >/dev/null 2>&1; then
            wget -q "$1" -O "$2"
        else
            die "curl or wget is required"
        fi
    }

    ARCHIVE="gptp-gm-$TARGET.tar.gz"
    say "Downloading gptp-gm ($VERSION, $TARGET)..."
    fetch "$BASE/$ARCHIVE" "$TMP/$ARCHIVE" || die "download failed: $BASE/$ARCHIVE"
    fetch "$BASE/$ARCHIVE.sha256" "$TMP/$ARCHIVE.sha256" || die "download failed: $BASE/$ARCHIVE.sha256"
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$TMP" && sha256sum -c "$ARCHIVE.sha256" >/dev/null) || die "checksum mismatch - download corrupted?"
    else
        say "(sha256sum not found, skipping checksum verification)"
    fi
    tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
fi

$SUDO mkdir -p "$BIN_DIR"
$SUDO install -m 0755 "$TMP/gptp-gm" "$BIN"
say "Installed $("$BIN" --version) to $BIN"

if [ -z "$IFACE" ]; then
    # Update: restart installed services on the new binary.
    restarted=""
    for svc in gptp-gm gptp-monitor; do
        if [ -e "$(unit_path "$svc")" ] && have_systemd; then
            $SUDO systemctl restart "$svc"
            restarted="$restarted $svc"
        fi
    done
    if [ -n "$restarted" ]; then
        say "Restarted:$restarted"
    else
        echo
        echo "Next: run it with the TUI            sudo gptp-gm eth0 [--handover 15 | --monitor]"
        echo "      or install it as a service     curl -fsSL https://github.com/$REPO/releases/latest/download/install.sh | sudo sh -s -- eth0 [options]"
    fi
    exit 0
fi

# --- Time sync (one-shot system configuration) ------------------------------------------------
if [ -n "$TIME_SYNC" ]; then
    setup_args=""
    for phc in $SYNC_PHCS; do setup_args="$setup_args --sync-phc $phc"; done
    say "Configuring time sync ($TIME_SYNC)..."
    # shellcheck disable=SC2086
    $SUDO "$BIN" "$IFACE" --setup-time-sync "$TIME_SYNC" $setup_args $SETUP_EXTRA \
        || die "time sync setup failed (see above)"
fi

# --- Service ---------------------------------------------------------------------------------
if [ "$MONITOR" -eq 1 ]; then
    SERVICE=gptp-monitor OTHER=gptp-gm OTHER_ARG=gm DESCRIPTION="gPTP slave monitor on $IFACE"
else
    SERVICE=gptp-gm OTHER=gptp-monitor OTHER_ARG=monitor DESCRIPTION="gPTP grandmaster on $IFACE"
fi
UNIT="$(unit_path "$SERVICE")"

[ -e "/sys/class/net/$IFACE" ] || say "warning: interface $IFACE does not exist right now - installing anyway"
if grep -qs "^ExecStart=$BIN $IFACE " "$(unit_path "$OTHER")"; then
    say "warning: $OTHER is also installed on $IFACE - both would answer peer delay on the same port; remove one with: --uninstall $OTHER_ARG"
fi

AFTER_EXTRA=""
CAPS_EXTRA=""
case "$TIME_SYNC" in
    builtin) CAPS_EXTRA=" CAP_SYS_TIME" ;;
    linuxptp) AFTER_EXTRA=" gptp-ptp4l-$IFACE.service" ;;
esac

RW_DIRS=""
[ -z "$METRICS_FILE" ] || RW_DIRS="$(dirname "$METRICS_FILE")"
[ -z "$CAPTURE_DIR" ] || RW_DIRS="${RW_DIRS:+$RW_DIRS }$CAPTURE_DIR"
RW_LINE=""
if [ -n "$RW_DIRS" ]; then
    # shellcheck disable=SC2086
    $SUDO mkdir -p $RW_DIRS
    RW_LINE="ReadWritePaths=$RW_DIRS"
fi

$SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=$DESCRIPTION
Documentation=https://github.com/$REPO
After=network-online.target sys-subsystem-net-devices-$IFACE.device$AFTER_EXTRA
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN $IFACE --no-tui$SERVICE_ARGS
# Runs as root for /dev/ptp* (PTP hardware clock) access, with capabilities
# bounded to raw sockets + NIC timestamping configuration (+ setting clocks
# with --time-sync builtin).
CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN$CAPS_EXTRA
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
$RW_LINE
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

$SUDO systemctl daemon-reload
$SUDO systemctl enable "$SERVICE" >/dev/null 2>&1
# restart, not start: a reinstall must pick up the new binary and arguments.
$SUDO systemctl restart "$SERVICE"

echo
$SUDO systemctl --no-pager --lines 0 status "$SERVICE" || true
echo
say "Installed $SERVICE.service"
echo "logs:      journalctl -fu $SERVICE"
[ -z "$METRICS_FILE" ] || echo "metrics:   $METRICS_FILE"
[ -z "$CAPTURE_DIR" ] || echo "captures:  $CAPTURE_DIR"
echo "remove:    curl -fsSL https://github.com/$REPO/releases/latest/download/install.sh | sudo sh -s -- --uninstall $([ "$MONITOR" -eq 1 ] && echo monitor || echo gm)"
