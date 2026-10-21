# gptp-gm — gPTP grandmaster with timed handover

[![CI](https://github.com/PlugOvr-ai/gptp/actions/workflows/ci.yml/badge.svg)](https://github.com/PlugOvr-ai/gptp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/PlugOvr-ai/gptp)](https://github.com/PlugOvr-ai/gptp/releases/latest)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A Rust gPTP (IEEE 802.1AS) grandmaster for vehicle networks, e.g. on a
Raspberry Pi. It serves time on one Ethernet interface so that all other
devices in the network take over the time signal, and can hand the
grandmaster role over to the automotive Ethernet switch after a
configurable number of minutes. A slave monitor mode watches an existing
master's time for jumps and instability instead.

## What it does

- **Forced grandmaster** on one interface: sends gPTP **Announce** (1/s) and
  two-step **Sync/Follow_Up** (8/s, the 802.1AS defaults) over raw Ethernet
  (EtherType `0x88F7`, link-local multicast `01:80:C2:00:00:0E`,
  majorSdoId 1), including the 802.1AS path-trace and Follow_Up information
  TLVs.
- **Highest priority**: announces `priority1 = 1` by default. BMCA compares
  priority1 first and lower wins, so this overrules any switch or endpoint
  using the common defaults (246–255). If another master on the network would
  still outrank us, the TUI shows a red warning.
- **Timed handover** (`--handover <minutes>`): after x minutes the program
  stops sending Announce/Sync. The other devices' announce-receipt timeout
  (3 announce intervals ≈ 3 s) then makes the BMCA elect the next-best
  master — the automotive Ethernet switch. The peer-delay responder stays
  active so neighbors keep their `asCapable` state across the handover.
- **Peer delay**: answers `Pdelay_Req` from neighbors (two-step) and runs its
  own peer-delay measurement once per second; the measured link delay is
  shown in the TUI.
- **TUI status console** (ratatui): role banner (ACTIVE GRANDMASTER /
  PASSIVE), handover countdown gauge, clock identity/priorities, TAI time
  source, message counters, other masters seen on the network (with BMCA
  verdict), and an event log.
  Keys: `q` quit, `g` toggle GM on/off manually, `r` restart the handover
  countdown (reactivates the GM if already handed over).
- **Node inventory with latency probing**: passively learns the MAC/IP of
  every device that sends any traffic on the segment, and probes each known
  IP once per second — ICMP echo, falling back to ARP-request RTT for nodes
  that ignore ICMP. The TUI shows per node: IP, MAC, last/average RTT,
  jitter, probe loss, and last-seen age; jitter above 1 ms is flagged red.
  `--scan-subnet 10.0.0.0/24` additionally ARP-sweeps a subnet every 60 s to
  find silent nodes; `--no-inventory` turns the feature off.
  **Node names** come from four sources, curated names always winning:
  a `--names` file (`<mac|ip> = <display name>` per line — recommended, since
  a vehicle network is fixed), DHCP hostname snooping (option 12), mDNS
  (passive announcements plus an active reverse lookup for unnamed nodes,
  once a minute), and LLDP system name (which in practice names the switch).
  Closed sensors that speak none of these can only be named via the names
  file. **Interpretation
  caveat:** RTT/jitter are *network + node stack* latency, deliberately not a
  claim about a node's gPTP sync accuracy (silent 802.1AS slaves don't expose
  their offset). What the numbers are good for: reachability, gross path
  problems, and congestion — RTT jitter spikes correlate with switch queue
  congestion, which is also what inflates gPTP residence times and hurts
  sync.
- **Link health**: `/sys/class/net/<if>/statistics` sampled every second —
  rx/tx rate plus drop/error counters. CRC/frame errors (cabling, connectors,
  PHY — invisible to ping) and missed/fifo errors (receiver overload) raise
  ALARM events with a hint; totals stay visible in the TUI.
- **Stream drop detection**: RTP, IEEE 1722/AVTP, and SOME/IP streams carry
  sequence counters; the tool tracks every such stream it can see and counts
  actual lost packets per stream — no sensor cooperation needed. Detection
  is heuristic, so a stream must first prove itself with a run of strictly
  consecutive counters before losses are counted. Loss raises rate-limited
  ALARM events. Note: only traffic that reaches our port is measurable —
  multicast/flooded streams usually are; for unicast sensor-to-ECU paths,
  mirror the relevant switch port onto ours.
- **Metrics logging** (`--log-metrics <file>`): appends a JSONL snapshot
  every 5 s (role, gPTP counters, link errors, per-node RTT/loss/load,
  per-stream loss) with single rotation at 50 MB. Vehicle faults are
  intermittent — this lets you correlate "sensor X desynced at 14:32:05"
  with the network state at that second, after the drive.
- **Slave monitor mode** (`--monitor`): instead of serving time, act as a
  passive gPTP slave — follow the best master on the link (BMCA over the
  received Announces, 1.5 s settle at startup) and watch its time. No clock
  is adjusted unless `--time-sync builtin` (below). Each Sync/Follow_Up
  gives one sample of *master time − local
  time* against a free-running local timebase (raw PHC with hardware
  timestamps, `CLOCK_MONOTONIC_RAW` otherwise — so NTP/chrony slewing the
  system clock never looks like a master problem). A robust (Theil–Sen)
  line fit over the last 8 s models the frequency difference — disturbed
  samples cannot bend it; what it does not predict is reported. A master
  noisier than the jump threshold (e.g. a GM converting each stamp from
  its system clock) is still learned after a few restarts: it is reported
  as noisy with its σ, the state shows UNSTABLE, and spike/step detection
  then needs 6σ until the noise is gone.
  - **STEP** — three consecutive samples off the model by the same amount:
    the master's time jumped (GM change, GM stepped its clock, servo
    reset). Size is reported exactly, even for jumps of years; the model is
    re-based and keeps running. The alarm names the sender's MAC and which
    part of the Sync moved: the GM's origin timestamp or the bridges'
    correctionField (each extrapolated from the samples before the step,
    so a bridge relaying a stale origin is handled), and whether the
    sender changed.
  - **spike** — a single sample off the model (delayed timestamp, usually
    bridge queueing). Logged at most every 10 s, all counted.
  - **frequency change** — the fitted drift leaves its long-term average.
  - **unstable** — RMS residual above ¼ of the jump threshold (hysteresis).
  - **erratic** — 8 consecutive samples fit neither the model nor each
    other; the model is discarded and relearned.
  - plus Sync timeouts (3 intervals), sequence gaps, missing Follow_Ups,
    GM changes (Announce, including a new GM relayed by the same bridge
    port) and `gmTimeBaseIndicator` changes (Follow_Up TLV).

  A steady phase ramp (e.g. an abrupt GM frequency change) is told apart
  from a step and followed by shortening the fit window. During a jitter
  burst the model is frozen (measured against, not learned from — for at
  most 30 s), steps then need three samples agreeing within ¼ of the jump
  threshold, and step/frequency alarms are labelled as possibly path
  delay; the drift reference is re-taken after a burst. The peer link
  delay is the median of the last 15 measurements, so queueing during a
  disturbance does not leak into the time offset afterwards.

  **Diagnostics — why did the jitter go up?** Every Sync also carries
  context, which is tracked against its own long-term baseline:
  bridge residence time (`correctionField`), Sync arrival-interval and GM
  send-interval regularity, peer link delay, Follow_Up latency, the GM's
  rate ratio (Follow_Up TLV), link load and link error counters, plus the
  followed GM's Announce fields. Unusual values are logged on their own
  (rate-limited), e.g. `bridge residence time … typically …` (only when
  the residual shows the bridge did not compensate it),
  `irregular Sync arrival`, `peer link delay changed`, `GM announce
  changed: clockClass 6 → 7 (holdover, within spec)`. A **jitter burst**
  — the residual's 1-s jitter level > 3× its baseline; robust to single
  spikes, which are reported separately — is logged one second in with
  what coincided, e.g.

  ```
  WARNING: jitter burst — 1-s jitter level up to 1.568 ms vs baseline 3.43 µs; coincides with:
    Sync arrival jitter up to 639.99 µs (typically ±7.26 µs) — delay in the path;
    14 Syncs missing; Follow_Up delay up to 1.2 ms (typically 0.0)
  ```

  or `no correlated network factor — suspect the GM's time base/servo or
  local timestamping` when nothing in the network explains it.
  With `--capture-dir <dir>`, every alarm (burst, step, frequency change,
  GM/announce/time-base change, Sync loss, …) writes
  `sync-<UTC>.csv` with one row per Sync from ~30 s before to ~10 s after
  it (deviation from the model, correction, link delay, arrival/send
  interval deviation, sequence gaps, Follow_Up delay, rate ratio, link
  load/errors, event notes) — alarms close together share one file.

  The TUI shows a sync-monitor panel (state, GM, drift, residual,
  RMS/peak, counters, master time vs. system clock, bridge residence
  time) and a sparkline of |residual| per Sync. Thresholds default to
  1 µs / 100 ppb with hardware timestamps and 250 µs / 5000 ppb with
  software timestamps (`--jump-threshold <ns>`, `--freq-threshold <ppb>`).
  The peer-delay responder and initiator stay active — a bridge only sends
  Sync to asCapable ports. Status lines, events and the JSONL metrics
  (`"sync"` object) cover the monitor too.

  **Listen-only** (`--listen-only`, automatic when `ptp4l` is running or
  with `--time-sync linuxptp`): the monitor sends nothing at all. Two gPTP
  stacks on one port both answer the switch's peer-delay requests; the
  switch then treats the link as not asCapable and **stops sending Sync**,
  and ptp4l goes FAULTY with "rogue peer delay response". The NIC's
  timestamping configuration is also left alone when it already stamps
  PTP frames.
- **Time sync on the slave** (`--monitor --time-sync MODE`): keep this
  machine's clocks on the master's time.
  - **`builtin`** — gptp-gm does what ptp4l + phc2sys do, in one process.
    With hardware timestamps it disciplines the receiving **NIC hardware
    clock** (PHC) to the master directly; the **system clock** (in UTC) and
    further NIC clocks (`--sync-phc <iface>`, repeatable — e.g. a second
    port whose clock timestamps sensor data) follow it via kernel
    cross-timestamps. Without hardware timestamps it disciplines the
    system clock directly. Each clock has a PI servo (linuxptp's gains,
    scaled to the Sync interval); it steps at start, when the monitor
    classifies a master time step, and beyond 128 ms. Spikes and jitter
    bursts are not used (holdover), and a master time before 2020 or after
    2100 is never followed. The kernel gets the TAI offset and the
    "synchronized" status, so `timedatectl` reports a synchronized clock.
    The monitor keeps measuring against the NIC clock's *reconstructed
    free-running* time — every adjustment is kept in a ledger — so its
    jump/jitter analysis stays exact while the clock is steered. Pauses
    (with an ALARM) while another time daemon runs.
  - **`linuxptp`** — for machines whose NIC clocks are best kept by
    linuxptp (e.g. NVIDIA DRIVE platforms). gptp-gm checks the units set up
    by `--setup-time-sync linuxptp` and alarms when the system clock is far
    off the master's UTC (whole TAI−UTC seconds point to a system clock on
    TAI or a master announcing a wrong UTC offset); it runs listen-only.

  **UTC offset**: PTP time is TAI, the system clock UTC; TAI − UTC = 37 s
  since 2017 (10 s in 1972 + 27 leap seconds; leap seconds end by 2035).
  The master's announced offset is used if marked valid and plausible,
  otherwise `--utc-offset` (37). Our grandmaster now also marks its
  announced offset valid.

  **Setup** (`--setup-time-sync MODE [--sync-phc …] [--dry-run]`, as root,
  one-shot): disables conflicting time services (systemd-timesyncd,
  chrony, ntpd, other ptp4l/phc2sys/timemaster units — masked ones are left
  alone) and records them; for `linuxptp` it also writes
  `/etc/linuxptp/gptp-gm-<iface>.cfg` (802.1AS: L2, P2P, transportSpecific
  1, hardware timestamping where the NIC has a PHC, explicit `utc_offset`,
  own management sockets) and the units `gptp-ptp4l-<iface>`,
  `gptp-phc2sys-<iface>-sysclock` (explicit `-O -37`, so a master that
  announces 0 cannot put the system clock on TAI) and
  `gptp-phc2sys-<iface>-<nic>`. `--dry-run` prints every file and
  `systemctl` command without changing anything; `--remove-time-sync`
  undoes it all and re-enables what was disabled. The installer runs this
  for you (below).
- `--no-tui` runs headless and prints status lines (including per-node
  latency lines) — use this for a systemd service. ALARM events appear in
  both the TUI event log and the headless output.

## Install

On the Raspberry Pi (or any Linux machine — x86_64, ARM64, ARMv7, ARMv6;
static binary, no dependencies):

```sh
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh
```

To install and start it as a systemd service in one step, pass the
interface and the `gptp-gm` options:

```sh
# grandmaster, hands over to the switch after 15 minutes
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh -s -- eth0 --handover 15
# slave monitor with metrics and capture files
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh -s -- eth0 --monitor \
    --log-metrics /var/log/gptp/monitor.jsonl --capture-dir /var/log/gptp/captures
# slave that keeps this machine's clocks in sync itself (Pi-class machines)
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh -s -- eth0 --monitor --time-sync builtin
# multi-NIC machines: set up ptp4l + phc2sys, monitor listen-only beside it
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh -s -- mgbe0_0 --monitor \
    --time-sync linuxptp --sync-phc mgbe3_0
```

**Grandmaster on a machine without a battery-backed RTC** (e.g. a Raspberry Pi): the system clock is
wrong at boot, and keeping NTP running would step or slew the clock while the grandmaster serves —
every correction reaches the network. `--ntp-once` sets the clock once, then freezes it:

```
# wait for the router, then the internet, set the clock by NTP once, stop NTP, start the GM
curl -fsSL https://github.com/PlugOvr-ai/gptp/releases/latest/download/install.sh | sudo sh -s -- eth0 --handover 15 \
    --ntp-once 192.168.1.1 [--internet-check 1.1.1.1,google.com]
```

Before the grandmaster starts, a prep script (`ExecStartPre`) waits until the router answers, then
until the internet hosts answer (default `1.1.1.1,google.com`, which also proves DNS), starts
`systemd-timesyncd` until the clock is synchronized, stops it again, and writes the time to the RTC
(`hwclock -w`, so a reboot starts with a sane clock; with an RTC battery, so does a cold boot).
`systemd-timesyncd` is disabled at boot and only ever run by this script; its previous state is
recorded and restored by `--uninstall gm`. The wait is bounded by `GM_PREP_MAX_WAIT` in the unit
(default 600 s, `0` = forever) — on expiry the grandmaster starts anyway with an unsynced clock and
a `WARNING` in the journal, on the principle that a GM with the wrong time beats no GM. Check with
`journalctl -b -t gm-time-prep`. Not combinable with `--time-sync` (there the master sets the clock).

With `--time-sync` the installer runs `--setup-time-sync` (disabling
conflicting time services, recorded) and gives the service
`CAP_SYS_TIME` for `builtin`; `--uninstall` undoes the setup and
re-enables the disabled services.

The installer picks the binary for the CPU, verifies its SHA-256, and
installs it to `/usr/local/bin/gptp-gm`. Run it again without arguments to
update — installed services are restarted on the new version.
`--uninstall [gm|monitor]` removes the service(s), and the binary once no
service uses it. **Behind a proxy** it uses `https_proxy`/`http_proxy`, and
since `sudo` drops your environment it also checks `/etc/environment`,
`/etc/profile.d`, apt's and dnf/yum's proxy settings and the GNOME proxy
setting of the user who ran `sudo`. Environment overrides:
`GPTP_VERSION=v0.2.0` (pin a release), `GPTP_BIN_DIR`, `GPTP_DOWNLOAD_URL`
(mirror). Releases are published by pushing a `v*` tag
(`.github/workflows/release.yml`, after the full CI passed).

## Build from source

```sh
cargo build --release            # on the Pi itself
# or static binaries for any Linux, e.g. the Pi, from a PC (needs cross):
cross build --release --target aarch64-unknown-linux-musl     # 64-bit Pi OS
cross build --release --target armv7-unknown-linux-musleabihf # 32-bit Pi OS
```

The binary needs raw-socket capability, plus `CAP_NET_ADMIN` to enable NIC
hardware timestamping. To run it without sudo:

```sh
sudo setcap 'cap_net_raw,cap_net_admin+ep' target/release/gptp-gm
```

For the hardware-timestamping path it must also be able to open the PHC
device (`/dev/ptp*`), which is root-only by default — run as root (the
systemd service does), or grant access with a udev rule.

## Usage

```sh
# serve as grandmaster indefinitely on eth0, with TUI
gptp-gm eth0

# serve for 10 minutes, then hand over to the switch
gptp-gm eth0 --handover 10

# headless for systemd, 15 min handover
gptp-gm eth0 --handover 15 --no-tui

# be a slave and watch the switch's time for jumps/instability, with a log
gptp-gm eth0 --monitor --log-metrics /var/log/gptp-monitor.jsonl
```

Selected options (see `--help` for all):

| Option | Default | Meaning |
|---|---|---|
| `--handover <min>` | off | stop announcing after x minutes (fractions allowed) |
| `--monitor` | off | slave monitor mode: follow the master, report jumps/instability |
| `--jump-threshold <ns>` | 1000 / 250000 | monitor: spike/step threshold (HW / SW timestamps) |
| `--freq-threshold <ppb>` | 100 / 5000 | monitor: frequency-change threshold (HW / SW) |
| `--capture-dir <dir>` | off | monitor: per-Sync CSV around every alarm |
| `--listen-only` | auto | monitor: send nothing (automatic when ptp4l runs) |
| `--time-sync <builtin\|linuxptp>` | off | monitor: keep this machine's clocks on the master |
| `--sync-phc <iface>` | — | also sync this NIC's hardware clock (repeatable) |
| `--setup-time-sync <mode>` | — | one-shot system setup for the mode, then exit (root) |
| `--remove-time-sync` | — | undo the setup, restore disabled services |
| `--dry-run` | off | with setup/remove: print the changes only |
| `--priority1 <n>` | 1 | grandmasterPriority1, lower wins the BMCA |
| `--priority2 <n>` | 1 | grandmasterPriority2 |
| `--clock-class <n>` | 248 | advertised clockClass |
| `--time-source <n>` | 0xA0 | advertised timeSource (0x20 GPS, 0x50 NTP) |
| `--domain <n>` | 0 | gPTP domain |
| `--log-sync-interval <n>` | -3 | log2 Sync interval (-3 = 8/s) |
| `--utc-offset <s>` | 37 | UTC→TAI offset when the kernel TAI offset is unset |
| `--gm-clock <src>` | auto | time source: `phc-disciplined` with hardware timestamps, else `system`; or `phc` (free-running NIC clock) |
| `--force-sw-timestamps` | off | skip NIC hardware timestamping even if available |
| `--scan-subnet <CIDR>` | off | ARP-sweep this subnet (/22../30) to find silent nodes |
| `--no-inventory` | off | disable node discovery and latency probing |
| `--names <file>` | off | curated node names, one `<mac\|ip> = <name>` per line |
| `--log-metrics <file>` | off | append JSONL health snapshots every 5 s |

## Run as a systemd service

The installer (above) sets up a hardened systemd unit (capabilities
bounded to `CAP_NET_RAW` + `CAP_NET_ADMIN`, filesystem protections,
headless mode, restart on failure). From a source checkout,
`scripts/install-service.sh` does the same with a freshly built binary
(it builds, then runs `install.sh` with it). The first argument is the
interface; everything after it is passed through to `gptp-gm`:

```sh
sudo ./scripts/install-service.sh eth0 --handover 15
journalctl -fu gptp-gm                        # follow the status log
sudo ./scripts/install-service.sh --uninstall # remove service(s) + binary
```

With `--monitor` the script installs a separate `gptp-monitor` service, so
a grandmaster and a monitor can coexist on different interfaces (it warns
if both target the same one). The directory of a `--log-metrics` file
(absolute path) and a `--capture-dir` are created and made writable despite
`ProtectSystem=strict`:

```sh
sudo ./scripts/install-service.sh eth0 --monitor \
    --log-metrics /var/log/gptp/monitor.jsonl --capture-dir /var/log/gptp/captures
journalctl -fu gptp-monitor
sudo ./scripts/install-service.sh --uninstall monitor   # or: gm
```

The script always rebuilds (incrementally) before installing, and both
restart the service, so a reinstall picks up new code and arguments.

## Time and timestamping notes

- On a Pi without an RTC battery, `--ntp-once` (see Install) sets the clock
  from NTP once at boot andthen freezes it, so no NTP step or slew ever reaches
  the slaves.
- PTP uses the TAI timescale. If the kernel's TAI offset is configured (e.g.
  by chrony's `leapsectz right/UTC`), it is used directly; otherwise the
  program falls back to `CLOCK_REALTIME` plus `--utc-offset` (37 s since
  2017). Keep the Pi's system clock disciplined (GNSS/NTP) — this program
  distributes the system clock, it does not discipline it.
- **Hardware timestamping is used automatically** when the NIC supports it
  (e.g. Raspberry Pi 5 / CM4; check with `ethtool -T eth0`): frames are
  stamped in the MAC/PHY via `SO_TIMESTAMPING`. Sync origin (via the
  two-step Follow_Up), pdelay t1–t4: all hardware-stamped.
- **Grandmaster time source (`--gm-clock`).** By default (`auto`) the NIC's
  hardware clock (PHC) is the time: at start its frequency is matched to the
  system clock (2 s) and it is set to TAI, then a slow PI servo keeps it on
  the system clock (`phc-disciplined`, like `phc2sys`), and hardware stamps
  are sent as they are. `phc` sets it once and lets it run free (smoothest,
  but drifts from UTC with the NIC's crystal). `system` converts every
  stamp with a fresh kernel cross-timestamp (`PTP_SYS_OFFSET_PRECISE`, else
  `PTP_SYS_OFFSET`): where those are only µs-accurate, that noise and every
  NTP adjustment reach the network directly — slaves see µs jitter. Do not
  run `phc2sys` on the same NIC with the phc modes.
- **When only software timestamping is available** (e.g. Raspberry Pi 4's
  NIC has no PHC), the program says so clearly: a red **⚠ SW TIMESTAMPS
  ONLY** badge in the TUI title bar and Clock panel, `ts=SW-ONLY(!)` in
  every headless status line, and a startup warning in the event log with
  the reason. Software mode still uses kernel driver-level timestamps from
  the socket error queue (better than plain userspace sampling), but
  accuracy is limited to tens of microseconds — and strict 802.1AS peers may
  reject the link because the measured peer delay can exceed their
  `neighborPropDelayThresh` (often 800 ns by default; raise it on the switch
  or use hardware timestamping).

## Design notes

- The `statime` crate was evaluated. It is a sans-IO
  implementation aimed at full BMCA instance operation; for this device we
  need a deliberately non-standard *forced* grandmaster (never yields to the
  BMCA) plus a timed step-down, so the small 802.1AS subset involved
  (Announce, two-step Sync/Follow_Up with gPTP TLVs, peer-delay
  initiator/responder) is implemented directly in `src/ptp.rs` over a raw
  `AF_PACKET` socket. That keeps full control over priorities, timing, and
  the handover behavior in ~400 lines of protocol code.
- Handover is implemented as *going silent*, which is the standard-conform
  way for a master to disappear: downstream nodes time out our Announce and
  elect the switch. We keep answering peer delay so the links stay
  `asCapable` and the takeover is seamless.

## Testing without hardware

- **Unit tests** (`cargo test`): message encode/decode, BMCA ordering, the
  monitor's step/spike/ramp/jitter/relearn/hold classification, the
  burst diagnostics and captures, JSON metrics, TUI rendering.
- **Integration test** (`scripts/integration-test.sh [binary]`): real
  processes in a throwaway network namespace (root, or unprivileged via a
  user namespace) on veth pairs and a Linux bridge — GM handover with a
  +3 s time step observed by the monitor, and a netem jitter burst that the
  monitor must attribute to path delay.
- **Docker lab** (`lab/`, see [lab/README.md](lab/README.md)): separate
  containers for the grandmaster, a backup GM, an L2 "automotive switch"
  and the monitor; `lab/run.sh` injects jitter on the switch, fails and
  restores the GM, and checks what the monitor reports. Also the place to
  experiment by hand (`docker compose -f lab/compose.yaml …`, TUI included).
- **CI** (`.github/workflows/ci.yml`, every push/PR): rustfmt, clippy
  `-D warnings`, shellcheck; unit + integration tests and a release build
  natively on **x86_64 and aarch64** (Ubuntu 22.04 runners, so the binaries
  also run on Raspberry Pi OS Bookworm) with the binaries as downloadable
  artifacts; and the Docker lab.

All of these use software timestamping (veth has no PHC), so they verify
behavior, not nanosecond accuracy — that needs the real NIC. The frames
decode cleanly in `tcpdump -v` / Wireshark.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this work, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
