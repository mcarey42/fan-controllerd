//! BMC handoff guard and signal-driven shutdown flag.
//!
//! The contract: if the daemon ever calls `enable_manual()`, then no matter
//! how it exits (clean shutdown, error return, panic unwind, SIGTERM), the
//! BMC must end up back in automatic mode. We get this by:
//!   1. Wrapping IpmiFan in a `BmcGuard` whose `Drop` calls `restore_auto`.
//!   2. Marking the guard "engaged" only after `enable_manual` succeeds, so
//!      a guard that's never engaged (dry-run, or pre-engage error) does
//!      nothing on drop.
//!   3. Never calling `std::process::exit` — always returning ExitCode from
//!      main so destructors run.
//!   4. Leaving the default panic strategy (unwind) so destructors run on
//!      panic too.
//!
//! Caveat: SIGKILL and hard power loss cannot be caught. The BMC's own
//! firmware will revert to auto after its internal timeout in that case.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::flag;

use crate::ipmi_fan::IpmiFan;

pub struct BmcGuard {
    fan: IpmiFan,
    engaged: AtomicBool,
}

impl BmcGuard {
    pub fn new(fan: IpmiFan) -> Self {
        Self {
            fan,
            engaged: AtomicBool::new(false),
        }
    }

    pub fn fan(&self) -> &IpmiFan {
        &self.fan
    }

    /// Enable manual mode and mark the guard engaged.
    pub fn engage(&self) -> Result<(), crate::ipmi_fan::IpmiFanError> {
        self.fan.enable_manual()?;
        self.engaged.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::SeqCst)
    }
}

impl Drop for BmcGuard {
    fn drop(&mut self) {
        if !self.engaged.load(Ordering::SeqCst) {
            return;
        }
        // Try twice — the BMC is occasionally slow on the first command.
        for attempt in 1..=2 {
            match self.fan.restore_auto() {
                Ok(()) => {
                    tracing::info!("BMC fan control restored to automatic mode");
                    return;
                }
                Err(e) if attempt == 1 => {
                    tracing::warn!("restore_auto attempt 1 failed: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => {
                    tracing::error!(
                        "FAILED to restore BMC auto control after 2 attempts: {e}. \
                         The BMC will revert to auto on its own watchdog timeout."
                    );
                }
            }
        }
    }
}

/// Set up SIGTERM/SIGINT/SIGHUP handlers that flip an AtomicBool. The main
/// loop polls this between (and during) ticks for snappy shutdown.
pub fn install_signal_handlers() -> std::io::Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [SIGTERM, SIGINT, SIGHUP] {
        flag::register(sig, Arc::clone(&stop))?;
    }
    Ok(stop)
}

/// Sleep up to `total` but wake early if `stop` flips. Used between ticks
/// for snappy SIGTERM response.
pub fn sleep_interruptible(total: std::time::Duration, stop: &AtomicBool) {
    let step = std::time::Duration::from_millis(200);
    let mut remaining = total;
    while remaining > std::time::Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let chunk = remaining.min(step);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unengaged_guard_drops_without_calling_ipmi() {
        // dry_run IpmiFan would no-op anyway, but the engaged check should
        // skip even the dry-run debug log path.
        let g = BmcGuard::new(IpmiFan::new(true));
        assert!(!g.is_engaged());
        drop(g); // no panic, no spawn
    }

    #[test]
    fn engaged_dry_run_guard_drops_cleanly() {
        let g = BmcGuard::new(IpmiFan::new(true));
        g.engage().unwrap();
        assert!(g.is_engaged());
        drop(g);
    }
}
