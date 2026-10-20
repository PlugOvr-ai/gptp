# gPTP lab (Docker)

A small vehicle network in containers, for trying the grandmaster and the
slave monitor without hardware and for injecting faults on purpose:

```
  gm (priority1 1) ─────┐                              ┌── monitor (--monitor)
                        ├─ gm_side ── switch ── sensor_side
  gm-backup ────────────┘   10.77.1.0/24   │   10.77.2.0/24
  (priority1 200, 1 s off)                 └─ L2 bridge container: the
                                              "automotive switch"; faults
                                              are injected on its ports
```

- **gm** — the Raspberry Pi grandmaster. Extra options via `GM_ARGS`,
  e.g. `GM_ARGS="--handover 1" docker compose … up`.
- **gm-backup** — stands in for the switch's own grandmaster: lower
  priority, and its time is deliberately 1 s off (`--time-error 1`, a
  hidden fault-injection option) so a failover is visible as a time step.
- **switch** — bridges its two ports at layer 2 (forwarding the gPTP
  multicast). `tc netem` on its port toward the sensors delays frames
  *between* the GM's transmit timestamp and the monitor's receive
  timestamp, like a congested switch.
- **monitor** — `gptp-gm --monitor` on the sensor side, headless, with
  JSONL metrics and CSV captures in the `out` volume.
- **fwd-mask** — one-shot helper on the host: Docker's bridges drop the
  gPTP multicast `01:80:C2:00:00:0E` unless `group_fwd_mask` allows it.
  It needs `NET_ADMIN` in the host network namespace, i.e. rootful Docker.

All timestamps are software timestamps (veth has no PHC), so accuracy is
in the µs range; the lab tests behavior, not nanoseconds.

## Scripted scenarios

```sh
lab/run.sh           # build, run, check, tear down (~2 min after the first build)
lab/run.sh --keep    # leave the lab running afterwards
```

1. path-delay jitter on the switch (2 ms ± 1 ms for 4 s) → the monitor
   must report a jitter burst attributed to delay in the path, and its end
2. grandmaster failure (`gm` stopped) → Sync loss, GM change, +1 s step
3. grandmaster returns → GM change, −1 s step

Logs, metrics and capture CSVs are collected in `lab/out/<UTC time>/`.
The jitter scenario is skipped if the host kernel lacks netem
(`sudo modprobe sch_netem`).

## Playing by hand

```sh
docker compose -f lab/compose.yaml up -d --build
docker compose -f lab/compose.yaml logs -f monitor          # event log
docker compose -f lab/compose.yaml run --rm monitor-tui     # live TUI

# faults
PORT=$(docker compose -f lab/compose.yaml exec -T switch cat /run/lab/port-2)
docker compose -f lab/compose.yaml exec switch tc qdisc add dev $PORT root netem delay 2ms 1ms
docker compose -f lab/compose.yaml exec switch tc qdisc del dev $PORT root
docker compose -f lab/compose.yaml exec switch tc qdisc add dev $PORT root netem loss 20%
docker compose -f lab/compose.yaml stop gm      # GM failure
docker compose -f lab/compose.yaml start gm     # GM returns

docker compose -f lab/compose.yaml down -v
```

`/run/lab/port-1` is the switch port toward the GMs, `port-2` toward the
sensors.
