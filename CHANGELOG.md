# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.2] — 2026-05-22

### Changed
- Per-tick duty-change log lines demoted from INFO to DEBUG. Keeps the
  default journal output focused on lifecycle events (startup, shutdown,
  errors, ceiling trips). Enable with `RUST_LOG=debug` — see the new
  "Debugging" section in the README.

## [0.1.1] — 2026-05-22

### Added
- `slew.deadband_pct` (default 2): suppress duty writes smaller than this
  many percentage points. Quiets the journal during steady-state operation
  by swallowing the typical 1°C CPU temperature jitter that translates into
  1% duty wobble. Set to 0 to disable. Fixes
  [#1](https://github.com/mcarey42/fan-controllerd/issues/1).

## [0.1.0] — 2026-05-22

Initial release. Tested on a Dell PowerEdge R730xd running Debian 13
(trixie) on Proxmox VE.

### Added
- Piecewise-linear curve per sensor with slew-rate limiting (default
  +10%/tick rise, -3%/tick fall).
- hwmon sensor backend (coretemp + NVMe), label glob matching, one config
  entry can expand to N physical readings (hottest wins).
- IPMI sensor backend (`ipmitool sdr type temperature`), parsed and cached
  once per tick.
- Inlet-bias support: hot room adds an upward bump to all demanded duties.
- Hard per-sensor ceilings; any over-ceiling reading hands control back to
  the BMC and exits non-zero.
- BMC failsafe via `Drop` guard: clean shutdown, error return, panic
  unwind, or signal all restore BMC auto via `ipmitool raw 0x30 0x30 0x01 0x01`.
- `systemd` integration: `Type=notify`, `WatchdogSec=30`, `Restart=on-failure`.
- `.deb` packaging via `cargo-deb`, with `postinst` (enable on boot, don't
  start) and `prerm` (stop + defensive restore-auto) hooks.
- CLI flags: `--config`, `--check`, `--dry-run`, `--once`.
- 40 unit tests covering curve math, slew, merge, inlet bias, config
  parsing/validation, hwmon path resolution, and IPMI SDR parsing.
