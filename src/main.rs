use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use sd_notify::NotifyState;
use tracing_subscriber::{fmt, EnvFilter};

use fan_controllerd::config::Config;
use fan_controllerd::control::{
    apply_deadband, demand_for_sensor, inlet_bias_pct, merge_demands, slew_limit,
};
use fan_controllerd::ipmi_fan::IpmiFan;
use fan_controllerd::safety::{install_signal_handlers, sleep_interruptible, BmcGuard};
use fan_controllerd::sensors::{self, ipmi::IpmiCache};

#[derive(Parser, Debug)]
#[command(
    name = "fan-controllerd",
    about = "Dynamic fan controller for Dell R630/R730 via IPMI"
)]
struct Cli {
    /// Path to TOML config file.
    #[arg(short, long, default_value = "/etc/fan-controllerd/config.toml")]
    config: PathBuf,

    /// Validate config and exit (no sensor reads, no IPMI writes).
    #[arg(long)]
    check: bool,

    /// Don't enable manual mode and don't write fan duty — just log what
    /// would happen. Safe for testing on a live system.
    #[arg(long)]
    dry_run: bool,

    /// Run one tick and exit. Combine with --dry-run for a safe shake-out.
    #[arg(long)]
    once: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match real_main(cli) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::from(1)
        }
    }
}

fn real_main(cli: Cli) -> Result<ExitCode> {
    let cfg = Config::from_file(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    tracing::info!(
        tick_seconds = cfg.tick_seconds,
        sensors = cfg.sensors.len(),
        min_duty = cfg.slew.min_duty,
        max_duty = cfg.slew.max_duty,
        dry_run = cli.dry_run,
        once = cli.once,
        "config loaded"
    );

    if cli.check {
        println!(
            "config OK: {} sensor(s), tick={}s",
            cfg.sensors.len(),
            cfg.tick_seconds
        );
        return Ok(ExitCode::SUCCESS);
    }

    let stop = install_signal_handlers().context("installing signal handlers")?;
    let guard = BmcGuard::new(IpmiFan::new(cli.dry_run));

    // Engage manual mode (no-op in dry-run mode — guard stays disengaged).
    if !cli.dry_run {
        guard.engage().context("enabling IPMI manual fan control")?;
        tracing::info!("BMC manual fan control engaged");
    } else {
        tracing::warn!("DRY-RUN: not engaging manual mode; IPMI writes will be logged only");
    }

    let result = run_loop(&cfg, &guard, &stop, cli.once);

    // Always notify systemd we're stopping; the guard's Drop fires after this
    // function returns, restoring BMC auto.
    let _ = sd_notify::notify(false, &[NotifyState::Stopping]);

    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => {
            tracing::error!("control loop terminated: {e:#}");
            // Exit non-zero so systemd Restart=on-failure kicks in.
            Ok(ExitCode::from(1))
        }
    }
}

fn run_loop(cfg: &Config, guard: &BmcGuard, stop: &Arc<AtomicBool>, once: bool) -> Result<()> {
    let mut ipmi = IpmiCache::new();
    let mut current_duty: Option<u8> = None;
    let mut ticks_since_write: u32 = 0;
    let tick = Duration::from_secs(cfg.tick_seconds);

    let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    tracing::info!("fan-controllerd ready");

    while !stop.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        // 1. Refresh IPMI cache once per tick (errors are non-fatal — hwmon
        //    sensors can still drive control).
        if let Err(e) = ipmi.refresh() {
            tracing::warn!("ipmi refresh failed: {e}");
        }

        // 2. Compute inlet bias once per tick (depends on IPMI inlet temp).
        let inlet_temp = cfg.inlet_bias.as_ref().and_then(|b| ipmi.get(&b.sensor));
        let bias = inlet_bias_pct(inlet_temp, cfg.inlet_bias.as_ref());

        // 3. Read every sensor, check ceilings, compute demands.
        let mut demands: Vec<Option<u8>> = Vec::with_capacity(cfg.sensors.len());
        let mut summary: Vec<String> = Vec::with_capacity(cfg.sensors.len());
        let mut successful = 0;

        for s in &cfg.sensors {
            match sensors::read(&s.spec, &ipmi) {
                Ok(readings) => {
                    let hottest = sensors::hottest(&readings).unwrap();
                    successful += 1;

                    // Hard ceiling: trip out to BMC auto immediately.
                    if hottest.temp_c >= s.hard_ceiling_c {
                        return Err(anyhow!(
                            "sensor '{}' reading {:.1}C >= hard ceiling {}C ({}); \
                             tripping to BMC auto",
                            s.name,
                            hottest.temp_c,
                            s.hard_ceiling_c,
                            hottest.label
                        ));
                    }

                    let demand = demand_for_sensor(s, hottest.temp_c, bias, &cfg.slew);
                    demands.push(Some(demand));
                    summary.push(format!("{}={:.1}C->{}%", s.name, hottest.temp_c, demand));
                }
                Err(e) => {
                    tracing::warn!("sensor '{}' read failed: {e}", s.name);
                    demands.push(None);
                    summary.push(format!("{}=ERR", s.name));
                }
            }
        }

        // 4. Merge into a single target duty.
        let target = merge_demands(&demands, &cfg.slew);

        // 5. Slew-limit toward target (first tick has no "current", so we
        //    write the target directly — no need to ease in from unknown).
        let slewed = match current_duty {
            None => target,
            Some(curr) => slew_limit(curr, target, &cfg.slew),
        };

        // 5a. Deadband: if the slew-limited proposal is within deadband_pct
        //     of the current duty, hold steady. Suppresses 1°C jitter chatter.
        let new_duty = apply_deadband(current_duty, slewed, cfg.slew.deadband_pct);

        // 6. Write if changed, or if heartbeat is due.
        let changed = current_duty != Some(new_duty);
        let heartbeat_due = ticks_since_write >= cfg.write_heartbeat_ticks;
        if changed || heartbeat_due {
            match guard.fan().set_duty(new_duty) {
                Ok(()) => {
                    // DEBUG (not INFO) because per-tick duty changes are
                    // chatty on a healthy idle box. Enable with RUST_LOG=debug
                    // when investigating curve tuning — see README.
                    if changed {
                        tracing::debug!(
                            duty = new_duty,
                            target = target,
                            inlet_bias = bias,
                            "duty {} -> {} ({}) [{}]",
                            current_duty
                                .map(|x| x.to_string())
                                .unwrap_or_else(|| "init".into()),
                            new_duty,
                            if heartbeat_due {
                                "changed+heartbeat"
                            } else {
                                "changed"
                            },
                            summary.join(" ")
                        );
                    } else {
                        tracing::debug!(duty = new_duty, "heartbeat write");
                    }
                    ticks_since_write = 0;
                    current_duty = Some(new_duty);
                }
                Err(e) => {
                    tracing::error!("set_duty({new_duty}) failed: {e}");
                    // Don't update current_duty — retry next tick.
                    ticks_since_write = ticks_since_write.saturating_add(1);
                }
            }
        } else {
            tracing::debug!(
                duty = new_duty,
                target = target,
                "tick (no write) [{}]",
                summary.join(" ")
            );
            current_duty = Some(new_duty);
            ticks_since_write = ticks_since_write.saturating_add(1);
        }

        // 7. Warn loudly if no sensor produced a reading. (Don't trip yet —
        //    duty falls back to min_duty via merge_demands. A persistent
        //    all-fail should be addressed by external monitoring.)
        if successful == 0 {
            tracing::warn!(
                "ALL {} sensor(s) failed this tick; holding at min_duty={}",
                cfg.sensors.len(),
                cfg.slew.min_duty
            );
        }

        // 8. Kick the systemd watchdog.
        let _ = sd_notify::notify(false, &[NotifyState::Watchdog]);

        if once {
            break;
        }

        // 9. Sleep until next tick (interruptible by SIGTERM).
        let elapsed = tick_start.elapsed();
        if elapsed < tick {
            sleep_interruptible(tick - elapsed, stop);
        }
    }

    tracing::info!("shutdown requested");
    Ok(())
}
