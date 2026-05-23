//! Read IPMI SDR temperatures by shelling out to `ipmitool`.
//!
//! We don't want to fork once per sensor — the BMC is slow. Instead, the
//! main loop calls `IpmiCache::refresh()` once per tick (one `ipmitool` call)
//! and individual sensor reads hit the in-memory map.
//!
//! Output format (pipe-delimited) looks like:
//!     Inlet Temp       | 04h | ok  |  7.1 | 20 degrees C
//!     Exhaust Temp     | 01h | ok  |  7.1 | 33 degrees C
//!     Temp             | 0Eh | ok  |  3.1 | 39 degrees C
//!     Power Supply 1   | C2h | ns  |  10.1 | No Reading
//!
//! We accept rows where status is "ok" and the value parses as
//! "<N> degrees C" (integer or float). Everything else (ns, na, disabled,
//! No Reading) is treated as missing.

use std::collections::HashMap;
use std::process::Command;

use super::SensorError;

const IPMITOOL: &str = "ipmitool";

#[derive(Debug, Default)]
pub struct IpmiCache {
    entries: HashMap<String, f32>,
    /// True once `refresh()` has succeeded at least once. We use this to
    /// distinguish "not yet polled" from "polled but sensor missing".
    pub ever_refreshed: bool,
}

impl IpmiCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `ipmitool sdr type temperature` and repopulate the cache.
    pub fn refresh(&mut self) -> Result<(), SensorError> {
        let output = Command::new(IPMITOOL)
            .args(["sdr", "type", "temperature"])
            .output()
            .map_err(|e| SensorError::Ipmi(format!("spawn ipmitool: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SensorError::Ipmi(format!(
                "ipmitool exited {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        self.entries = parse_sdr(&stdout);
        self.ever_refreshed = true;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<f32> {
        self.entries.get(name).copied()
    }

    #[cfg(test)]
    fn from_text(s: &str) -> Self {
        Self {
            entries: parse_sdr(s),
            ever_refreshed: true,
        }
    }
}

/// Parse the pipe-delimited output of `ipmitool sdr type temperature`.
/// Duplicate names (e.g. multiple unnamed "Temp" rows) use last-wins; the
/// daemon should be configured to reference uniquely-named sensors only.
fn parse_sdr(text: &str) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        if fields.len() < 5 {
            continue;
        }
        let name = fields[0];
        let status = fields[2];
        let value = fields[4];
        if !status.eq_ignore_ascii_case("ok") {
            continue;
        }
        if let Some(temp_c) = parse_temp(value) {
            out.insert(name.to_string(), temp_c);
        }
    }
    out
}

/// "20 degrees C" -> Some(20.0); "23.5 degrees C" -> Some(23.5);
/// "No Reading" / "disabled" / "na" -> None.
fn parse_temp(value: &str) -> Option<f32> {
    let v = value.trim();
    let num_part = v.split_whitespace().next()?;
    num_part.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_OUTPUT: &str = "\
Inlet Temp       | 04h | ok  |  7.1 | 20 degrees C
Exhaust Temp     | 01h | ok  |  7.1 | 33 degrees C
Temp             | 0Eh | ok  |  3.1 | 39 degrees C
Temp             | 0Fh | ok  |  3.2 | 41 degrees C
";

    #[test]
    fn parses_real_r730xd_output() {
        let cache = IpmiCache::from_text(REAL_OUTPUT);
        assert_eq!(cache.get("Inlet Temp"), Some(20.0));
        assert_eq!(cache.get("Exhaust Temp"), Some(33.0));
        // Duplicate "Temp" rows — last wins.
        assert_eq!(cache.get("Temp"), Some(41.0));
        assert_eq!(cache.get("Nonexistent"), None);
    }

    #[test]
    fn skips_non_ok_status_rows() {
        let text = "\
Bad Sensor       | 05h | ns  |  7.1 | No Reading
Good Sensor      | 06h | ok  |  7.1 | 25 degrees C
Also Bad         | 07h | nr  |  7.1 | 99 degrees C
";
        let cache = IpmiCache::from_text(text);
        assert_eq!(cache.get("Bad Sensor"), None);
        assert_eq!(cache.get("Also Bad"), None);
        assert_eq!(cache.get("Good Sensor"), Some(25.0));
    }

    #[test]
    fn parses_fractional_temps() {
        let text = "Foo | 01h | ok | 7.1 | 23.5 degrees C\n";
        let cache = IpmiCache::from_text(text);
        assert_eq!(cache.get("Foo"), Some(23.5));
    }

    #[test]
    fn ignores_malformed_lines() {
        let text = "garbage line with no pipes\n| | ok | | 25 degrees C\nGood | 01h | ok | 7.1 | 30 degrees C\n";
        let cache = IpmiCache::from_text(text);
        // garbage skipped, empty-name row stored under empty key, Good present.
        assert_eq!(cache.get("Good"), Some(30.0));
    }

    #[test]
    fn handles_disabled_or_na_values() {
        let text = "\
Foo | 01h | ok | 7.1 | disabled
Bar | 02h | ok | 7.1 | na
Baz | 03h | ok | 7.1 | 42 degrees C
";
        let cache = IpmiCache::from_text(text);
        assert_eq!(cache.get("Foo"), None);
        assert_eq!(cache.get("Bar"), None);
        assert_eq!(cache.get("Baz"), Some(42.0));
    }
}
