# fan-controllerd

Dynamic fan controller daemon for Dell PowerEdge R630 / R730 / R730xd servers.

The Dell BMC defaults to a conservative fan curve that's loud at idle (R730xd
fans spin at ~15,000 RPM even with CPUs at 38°C). This daemon takes manual
control via IPMI and runs the fans on a temperature-driven curve — quiet at
idle (~4,800 RPM, 20% duty), responsive to load spikes, slew-rate limited so
nothing hunts or howls.

## Tested hardware

- Dell PowerEdge R730xd, Debian 13 (trixie), Proxmox VE host

Should work on:

- R630, R730 (uses identical Dell-OEM IPMI commands `0x30 0x30 ...`)
- Any Debian-family Linux (bookworm, trixie, Ubuntu 22.04+)

Will **not** work on:

- Non-Dell servers (the raw IPMI bytes are Dell-specific)
- Dell servers without iDRAC Express/Enterprise on the same generation

## Install

Grab the `.deb` from the latest [release](../../releases/latest) and install it:

```sh
sudo dpkg -i fan-controllerd_*.deb
```

The postinst script enables the service on boot but does **not** start it —
review the config first, then start it manually.

```sh
# 1. Review and (probably) edit the config
sudo nano /etc/fan-controllerd/config.toml

# 2. Validate it parses
sudo fan-controllerd --check

# 3. Safe shake-out: one tick, no IPMI writes
sudo fan-controllerd --dry-run --once

# 4. Start it for real
sudo systemctl start fan-controllerd

# 5. Watch it work
journalctl -fu fan-controllerd
ipmitool sdr type fan
```

To stop and hand control back to the BMC:

```sh
sudo systemctl stop fan-controllerd
```

## How it works

Each tick (default 5 s) the daemon:

1. **Reads sensors** — CPU package temps from `/sys/class/hwmon` (coretemp),
   NVMe temps from hwmon, IPMI exhaust + inlet from a single
   `ipmitool sdr type temperature` call (cached for the tick).
2. **Computes per-sensor demand** — each `[[sensor]]` has its own
   piecewise-linear curve (temp → duty %). Interpolated; below the curve →
   the curve's first duty, above → the last.
3. **Applies inlet bias** — if the inlet sensor exceeds the configured
   threshold, all demands get a small upward bump (a hot room shouldn't
   expect cold exhaust).
4. **Merges across sensors — loudest wins.** The fan duty is set by whichever
   sensor demands the most cooling that tick.
5. **Slew-rate limits** — caps how fast duty can change per tick (default
   +10% rise, −3% fall). This is the hysteresis-equivalent that prevents
   audible hunting on small temp jitter.
6. **Writes** — `ipmitool raw 0x30 0x30 0x02 0xff <duty_hex>` if the duty
   changed, or every N ticks as a heartbeat (so the BMC doesn't silently
   revert).

## Safety properties

The daemon goes out of its way to never leave the BMC stuck in manual mode
with no daemon controlling it:

- **Drop guard on every exit.** On clean shutdown, error return, panic
  unwind, SIGTERM, SIGINT, or SIGHUP — `ipmitool raw 0x30 0x30 0x01 0x01`
  fires to restore BMC automatic mode. Two attempts with a backoff.
- **Hard ceilings.** Each sensor has a `hard_ceiling_c`. If any reading
  meets or exceeds it, the daemon hands control back to the BMC and exits
  non-zero. systemd `Restart=on-failure` brings it back.
- **systemd watchdog.** `Type=notify` with `WatchdogSec=30` — if the tick
  loop hangs, systemd kills the process (which triggers the Drop guard).
- **Heartbeat writes.** Duty re-sent every ~60 s even if unchanged, so the
  BMC can't auto-revert during a long quiet stretch.
- **Conffile protection.** `/etc/fan-controllerd/config.toml` is marked as a
  dpkg conffile — your edits survive package upgrades.
- **Defense in depth.** The package's `prerm` script issues a defensive
  "restore auto" command, in case the daemon was killed before its Drop
  guard could fire (OOM, SIGKILL, etc).

Caveat: SIGKILL and hard power loss can't be caught. The BMC's own firmware
will revert to auto after its internal timeout in that case.

## Configuration

`/etc/fan-controllerd/config.toml` ships with sensible defaults for an R730xd
with dual sockets, 8 NVMe drives, and IPMI exhaust/inlet. Tune the curves
for your workload:

```toml
tick_seconds = 5
write_heartbeat_ticks = 12     # re-send duty every ~60s

[slew]
max_rise_per_tick = 10         # ramp up fast on load spike
max_fall_per_tick = 3          # come down slowly to avoid hunting
min_duty = 20                  # quiet floor
max_duty = 100

[inlet_bias]
sensor = "Inlet Temp"
threshold_c = 27.0
percent_per_degree_above = 2.0
max_bias_pct = 30

[[sensor]]
name = "cpu-pkg-0"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 90.0
curve = [[40, 20], [55, 25], [65, 35], [75, 55], [85, 90]]

# ...
```

See [`config/config.toml.example`](config/config.toml.example) for the full
annotated default config.

### Curve tuning

- `curve = [[temp_c, duty_pct], ...]` — must be sorted ascending by temp,
  ≥ 2 points. Below the first point → first duty, above the last → last duty.
- `hard_ceiling_c` must be **above** the curve's top temperature (otherwise
  the daemon would trip out before the curve ever asks for max duty).
- `min_duty` is your quiet floor. 20% is silent on R730xd. Drop to 15 if you
  want quieter and have good airflow.

### Sensor discovery

```sh
# What hwmon chips and labels does your system expose?
for h in /sys/class/hwmon/hwmon*; do
  echo "=== $(cat $h/name) ==="
  for f in $h/temp*_input; do
    lbl=${f%_input}_label
    printf "  %-30s = %s mC\n" "$([ -r $lbl ] && cat $lbl)" "$(cat $f)"
  done
done

# What IPMI sensors are available?
ipmitool sdr type temperature
```

Use the chip name (e.g. `coretemp`, `nvme`) and label (e.g. `Package id 0`,
`Composite`) in your config. Labels support a trailing `*` wildcard
(e.g. `label = "Package id *"`).

### Multiple readings per spec

A single `[[sensor]]` entry can yield multiple physical readings — for
example, `chip = "nvme", label = "Composite"` matches all NVMe drives in the
system. The hottest reading drives that sensor's curve.

## Build from source

Needs Rust 1.70+ and (for `.deb`) `cargo-deb`.

```sh
cargo build --release                       # binary in target/release/
cargo test                                  # 40+ unit tests
cargo install cargo-deb && cargo deb        # .deb in target/debian/
```

## License

MIT — see [LICENSE](LICENSE).
