# gptp-gm — gPTP grandmaster with timed handover

A Rust gPTP (IEEE 802.1AS) grandmaster for replacing the Elproma NTS Pico3
mini time server in our ADAS vehicles with e.g. a Raspberry Pi. It serves
time on one Ethernet interface so that all other devices in the network take
over the time signal, and can hand the grandmaster role over to the
automotive Ethernet switch after a configurable number of minutes.

## Original requirements

> we have currently a NTC Pico in our ADAS vehicles (https://www.lange-electronic.com/de/elproma-nts-pico3-miniatur-timeserver).
> we like to replace it with e.g. a raspberry pi that runs a gptp grandmaster and all other devices in the network shall take over the time signal.
> The priority of the time signal that we sends need to be the highest, so that in case there is another time master in the network its overruled.
> We also have a automotive ethernet switch that initialized itself at boot up and then sends the time signal to the corresponding sensor devices.
> I want a mode added where you only send time signals for x minutes and after that the ethernet switch will serve as grand master.
> develop the programm in rust. statime could be a useful crate for you.
> add a TUI console ui that shows the current status.

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
  is adjusted. Each Sync/Follow_Up gives one sample of *master time − local
  time* against a free-running local timebase (raw PHC with hardware
  timestamps, `CLOCK_MONOTONIC_RAW` otherwise — so NTP/chrony slewing the
  system clock never looks like a master problem). A linear fit over the
  last 8 s models the frequency difference; what it does not predict is
  reported:
  - **STEP** — two consecutive samples off the model by the same amount:
    the master's time jumped (GM change, GM stepped its clock, servo
    reset). Size is reported exactly, even for jumps of years; the model is
    re-based and keeps running.
  - **spike** — a single sample off the model (delayed timestamp, usually
    bridge queueing). Logged at most every 10 s, all counted.
  - **frequency change** — the fitted drift leaves its long-term average.
  - **unstable** — RMS residual above ¼ of the jump threshold (hysteresis).
  - **erratic** — 8 consecutive samples fit neither the model nor each
    other; the model is discarded and relearned.
  - plus Sync timeouts (3 intervals), sequence gaps, missing Follow_Ups,
    GM changes (Announce) and `gmTimeBaseIndicator` changes (Follow_Up TLV).

  The TUI shows a sync-monitor panel (state, GM, drift, residual,
  RMS/peak, counters, master time vs. system clock, bridge residence
  time) and a sparkline of |residual| per Sync. Thresholds default to
  1 µs / 100 ppb with hardware timestamps and 250 µs / 5000 ppb with
  software timestamps (`--jump-threshold <ns>`, `--freq-threshold <ppb>`).
  The peer-delay responder and initiator stay active — a bridge only sends
  Sync to asCapable ports. Status lines, events and the JSONL metrics
  (`"sync"` object) cover the monitor too.
- `--no-tui` runs headless and prints status lines (including per-node
  latency lines) — use this for a systemd service. ALARM events appear in
  both the TUI event log and the headless output.

## Build

```sh
cargo build --release            # on the Pi itself
# or cross-compile from a PC:
cargo build --release --target aarch64-unknown-linux-gnu
```

The binary needs raw-socket capability, plus `CAP_NET_ADMIN` to enable NIC
hardware timestamping:

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
| `--priority1 <n>` | 1 | grandmasterPriority1, lower wins the BMCA |
| `--priority2 <n>` | 1 | grandmasterPriority2 |
| `--clock-class <n>` | 248 | advertised clockClass |
| `--time-source <n>` | 0xA0 | advertised timeSource (0x20 GPS, 0x50 NTP) |
| `--domain <n>` | 0 | gPTP domain |
| `--log-sync-interval <n>` | -3 | log2 Sync interval (-3 = 8/s) |
| `--utc-offset <s>` | 37 | UTC→TAI offset when the kernel TAI offset is unset |
| `--force-sw-timestamps` | off | skip NIC hardware timestamping even if available |
| `--scan-subnet <CIDR>` | off | ARP-sweep this subnet (/22../30) to find silent nodes |
| `--no-inventory` | off | disable node discovery and latency probing |
| `--names <file>` | off | curated node names, one `<mac\|ip> = <name>` per line |
| `--log-metrics <file>` | off | append JSONL health snapshots every 5 s |

## Run as a systemd service

`scripts/install-service.sh` builds the release binary if needed, installs it
to `/usr/local/bin/gptp-gm`, and generates + enables a hardened systemd unit
(capabilities bounded to `CAP_NET_RAW` + `CAP_NET_ADMIN`, filesystem
protections, headless mode, restart on failure). The first argument is the
interface; everything after it is passed through to `gptp-gm`:

```sh
sudo ./scripts/install-service.sh eth0 --handover 15
journalctl -fu gptp-gm                        # follow the status log
sudo ./scripts/install-service.sh --uninstall # remove service(s) + binary
```

With `--monitor` the script installs a separate `gptp-monitor` service, so
a grandmaster and a monitor can coexist on different interfaces (it warns
if both target the same one). The directory of a `--log-metrics` file
(absolute path) is created and made writable despite `ProtectSystem=strict`:

```sh
sudo ./scripts/install-service.sh eth0 --monitor --log-metrics /var/log/gptp/monitor.jsonl
journalctl -fu gptp-monitor
sudo ./scripts/install-service.sh --uninstall monitor   # or: gm
```

The script always rebuilds (incrementally) before installing, and
restarts the service, so a reinstall picks up new code and arguments.

## Time and timestamping notes

- PTP uses the TAI timescale. If the kernel's TAI offset is configured (e.g.
  by chrony's `leapsectz right/UTC`), it is used directly; otherwise the
  program falls back to `CLOCK_REALTIME` plus `--utc-offset` (37 s since
  2017). Keep the Pi's system clock disciplined (GNSS/NTP) — this program
  distributes the system clock, it does not discipline it.
- **Hardware timestamping is used automatically** when the NIC supports it
  (e.g. Raspberry Pi 5 / CM4; check with `ethtool -T eth0`): frames are
  stamped in the MAC/PHY via `SO_TIMESTAMPING`, and each stamp is converted
  from the PHC timebase to TAI with a kernel cross-timestamp
  (`PTP_SYS_OFFSET_PRECISE`, falling back to `PTP_SYS_OFFSET`) — no `phc2sys`
  needed. Sync origin (via the two-step Follow_Up), pdelay t1–t4: all
  hardware-stamped. Expect sub-microsecond wire accuracy.
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

- The `statime` crate was evaluated as suggested. It is a sans-IO
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

Unit tests cover message encode/decode and the BMCA ordering (`cargo test`).
A full two-instance integration test runs in an unprivileged user namespace
against a veth pair — instance A as forced GM with a 5 s handover, instance B
simulating the switch with priority1 200:

```sh
unshare -rn bash -c '
  ip link add vethA type veth peer name vethB
  ip link set vethA up; ip link set vethB up
  ./target/release/gptp-gm vethA --no-tui --handover 0.08 &
  ./target/release/gptp-gm vethB --no-tui --priority1 200 &
  sleep 10; kill -INT %1 %2'
```

Expected: A announces/syncs and hands over after ~5 s, B reports A as a
foreign master that outranks it, and both measure the peer link delay. The
frames decode cleanly in `tcpdump -v` / Wireshark.

Monitor mode across a GM change with a time jump: two GMs and a monitor on
a Linux bridge (which must be told to forward the gPTP multicast); GM C runs
3 s off via `--utc-offset`:

```sh
unshare -rn bash -c '
  ip link add br0 type bridge group_fwd_mask 0x4000
  for x in A C M; do ip link add veth$x type veth peer name p$x
    ip link set p$x master br0; ip link set p$x up; ip link set veth$x up; done
  ip link set br0 up
  ./target/release/gptp-gm vethA --no-tui --no-inventory --handover 0.1 &
  ./target/release/gptp-gm vethC --no-tui --no-inventory --priority1 200 --utc-offset 40 &
  ./target/release/gptp-gm vethM --no-tui --no-inventory --monitor &
  sleep 16; kill -INT %1 %2 %3'
```

Expected from the monitor: following A, then `Sync stopped` at the
handover, `grandmaster changed`, `Sync resumed`, and
`master time STEP +3.000000 s`.
