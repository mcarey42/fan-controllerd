//! Dell-OEM IPMI raw commands for fan control.
//!
//! Commands (R630/R730 family):
//!   - Enable manual mode:  `ipmitool raw 0x30 0x30 0x01 0x00`
//!   - Restore BMC auto:    `ipmitool raw 0x30 0x30 0x01 0x01`
//!   - Set duty (all fans): `ipmitool raw 0x30 0x30 0x02 0xff <duty_hex>`
//!     where <duty_hex> is the duty percent in hex (0x14 = 20%, 0x64 = 100%).

use std::process::Command;

use thiserror::Error;

const IPMITOOL: &str = "ipmitool";

#[derive(Debug, Error)]
pub enum IpmiFanError {
    #[error("failed to spawn ipmitool: {0}")]
    Spawn(String),
    #[error("ipmitool exited {status}: {stderr}")]
    CommandFailed {
        status: std::process::ExitStatus,
        stderr: String,
    },
}

pub struct IpmiFan {
    dry_run: bool,
}

impl IpmiFan {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Enable manual fan control. Must be called once before any `set_duty`.
    pub fn enable_manual(&self) -> Result<(), IpmiFanError> {
        self.run_raw(&["0x30", "0x30", "0x01", "0x00"])
    }

    /// Restore BMC automatic fan control. Idempotent — safe to call multiple
    /// times and safe to call without first calling `enable_manual`.
    pub fn restore_auto(&self) -> Result<(), IpmiFanError> {
        self.run_raw(&["0x30", "0x30", "0x01", "0x01"])
    }

    /// Set fan duty cycle as a percentage [0, 100]. Out-of-range values are
    /// clamped (defense in depth — the slew limiter already clamps).
    pub fn set_duty(&self, pct: u8) -> Result<(), IpmiFanError> {
        let pct = pct.min(100);
        let hex = format!("0x{pct:02x}");
        self.run_raw(&["0x30", "0x30", "0x02", "0xff", &hex])
    }

    fn run_raw(&self, args: &[&str]) -> Result<(), IpmiFanError> {
        if self.dry_run {
            tracing::debug!(args = ?args, "dry-run: would run ipmitool raw");
            return Ok(());
        }
        let output = Command::new(IPMITOOL)
            .arg("raw")
            .args(args)
            .output()
            .map_err(|e| IpmiFanError::Spawn(e.to_string()))?;
        if !output.status.success() {
            return Err(IpmiFanError::CommandFailed {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_never_spawns() {
        // If this spawned ipmitool, the test machine would (hopefully) lack
        // it and we'd get an error. dry-run must return Ok regardless.
        let fan = IpmiFan::new(true);
        fan.enable_manual().unwrap();
        fan.set_duty(35).unwrap();
        fan.set_duty(200).unwrap(); // clamps
        fan.restore_auto().unwrap();
    }
}
