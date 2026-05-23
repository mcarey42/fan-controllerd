use serde::Deserialize;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_tick_seconds")]
    pub tick_seconds: u64,

    /// Re-send duty to BMC every N ticks even if unchanged, so BMC doesn't
    /// silently revert to auto. Default 12 ticks (= 60s at 5s tick).
    #[serde(default = "default_heartbeat_ticks")]
    pub write_heartbeat_ticks: u32,

    #[serde(default)]
    pub slew: SlewConfig,

    #[serde(default)]
    pub ipmi: IpmiConfig,

    #[serde(rename = "sensor", default)]
    pub sensors: Vec<SensorConfig>,

    pub inlet_bias: Option<InletBiasConfig>,
}

fn default_tick_seconds() -> u64 {
    5
}
fn default_heartbeat_ticks() -> u32 {
    12
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlewConfig {
    #[serde(default = "default_rise")]
    pub max_rise_per_tick: u8,
    #[serde(default = "default_fall")]
    pub max_fall_per_tick: u8,
    #[serde(default = "default_min_duty")]
    pub min_duty: u8,
    #[serde(default = "default_max_duty")]
    pub max_duty: u8,
}

fn default_rise() -> u8 {
    10
}
fn default_fall() -> u8 {
    3
}
fn default_min_duty() -> u8 {
    20
}
fn default_max_duty() -> u8 {
    100
}

impl Default for SlewConfig {
    fn default() -> Self {
        Self {
            max_rise_per_tick: default_rise(),
            max_fall_per_tick: default_fall(),
            min_duty: default_min_duty(),
            max_duty: default_max_duty(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpmiConfig {
    // Local in-band only for now; future fields (host/user/password_file)
    // will go here.
}

#[derive(Debug, Clone, Deserialize)]
// NOTE: no `deny_unknown_fields` here — it's incompatible with `serde(flatten)`
// over a tagged enum. With it, the enum tag (`source`) gets rejected as
// "unknown" before being forwarded to the flattened SensorSpec.
pub struct SensorConfig {
    pub name: String,
    #[serde(flatten)]
    pub spec: SensorSpec,
    pub hard_ceiling_c: f32,
    /// Pairs of (temp_c, duty_pct). Must be sorted ascending by temp_c and
    /// contain at least two points.
    pub curve: Vec<(f32, u8)>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SensorSpec {
    /// Read from /sys/class/hwmon. `chip` matches the hwmon `name` file
    /// (e.g. "coretemp", "nvme"). `label` matches the `tempN_label` file
    /// and supports a trailing `*` glob (e.g. "Package id *", "Composite").
    Hwmon { chip: String, label: String },
    /// Read from `ipmitool sdr type temperature`. `sensor` matches the
    /// SDR sensor name exactly (e.g. "Exhaust Temp", "Inlet Temp").
    Ipmi { sensor: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InletBiasConfig {
    /// IPMI SDR name of the inlet sensor (e.g. "Inlet Temp").
    pub sensor: String,
    /// Below this, no bias is applied.
    pub threshold_c: f32,
    /// Duty% added per degree the inlet exceeds threshold_c.
    pub percent_per_degree_above: f32,
    /// Cap on total added duty% (default 30).
    #[serde(default = "default_bias_cap")]
    pub max_bias_pct: u8,
}

fn default_bias_cap() -> u8 {
    30
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_str = path.as_ref().display().to_string();
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        Self::from_toml_str(&text).map_err(|e| match e {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path_str.clone(),
                source,
            },
            other => other,
        })
    }

    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: "<inline>".into(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.tick_seconds == 0 {
            return Err(ConfigError::Validation("tick_seconds must be >= 1".into()));
        }
        if self.slew.min_duty > self.slew.max_duty {
            return Err(ConfigError::Validation(format!(
                "slew.min_duty ({}) > slew.max_duty ({})",
                self.slew.min_duty, self.slew.max_duty
            )));
        }
        if self.slew.max_duty > 100 {
            return Err(ConfigError::Validation(
                "slew.max_duty must be <= 100".into(),
            ));
        }
        if self.sensors.is_empty() {
            return Err(ConfigError::Validation(
                "at least one [[sensor]] is required".into(),
            ));
        }
        for s in &self.sensors {
            s.validate()?;
        }
        Ok(())
    }
}

impl SensorConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.curve.len() < 2 {
            return Err(ConfigError::Validation(format!(
                "sensor '{}' curve needs at least 2 points",
                self.name
            )));
        }
        for win in self.curve.windows(2) {
            if win[1].0 <= win[0].0 {
                return Err(ConfigError::Validation(format!(
                    "sensor '{}' curve temps must be strictly ascending (got {} then {})",
                    self.name, win[0].0, win[1].0
                )));
            }
        }
        for (t, d) in &self.curve {
            if *d > 100 {
                return Err(ConfigError::Validation(format!(
                    "sensor '{}' curve point ({}, {}) has duty > 100",
                    self.name, t, d
                )));
            }
        }
        if self.hard_ceiling_c <= self.curve.last().unwrap().0 {
            return Err(ConfigError::Validation(format!(
                "sensor '{}' hard_ceiling_c ({}) should be above the curve's top temp ({})",
                self.name,
                self.hard_ceiling_c,
                self.curve.last().unwrap().0,
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[[sensor]]
name = "cpu"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 90
curve = [[40, 20], [60, 40], [80, 80]]
"#;

    #[test]
    fn parses_minimal() {
        let cfg = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(cfg.tick_seconds, 5);
        assert_eq!(cfg.write_heartbeat_ticks, 12);
        assert_eq!(cfg.slew.min_duty, 20);
        assert_eq!(cfg.sensors.len(), 1);
        match &cfg.sensors[0].spec {
            SensorSpec::Hwmon { chip, label } => {
                assert_eq!(chip, "coretemp");
                assert_eq!(label, "Package id 0");
            }
            other => panic!("expected hwmon, got {:?}", other),
        }
    }

    #[test]
    fn rejects_empty_sensors() {
        let cfg = Config::from_toml_str("tick_seconds = 5\n").unwrap_err();
        assert!(matches!(cfg, ConfigError::Validation(_)));
    }

    #[test]
    fn rejects_unsorted_curve() {
        let bad = r#"
[[sensor]]
name = "cpu"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 90
curve = [[60, 40], [40, 20]]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(msg) if msg.contains("ascending")));
    }

    #[test]
    fn rejects_duty_over_100() {
        let bad = r#"
[[sensor]]
name = "cpu"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 90
curve = [[40, 20], [80, 120]]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(msg) if msg.contains("duty > 100")));
    }

    #[test]
    fn rejects_ceiling_at_or_below_curve_top() {
        let bad = r#"
[[sensor]]
name = "cpu"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 80
curve = [[40, 20], [80, 100]]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(msg) if msg.contains("hard_ceiling_c")));
    }

    #[test]
    fn parses_ipmi_sensor_and_inlet_bias() {
        let cfg_str = r#"
[inlet_bias]
sensor = "Inlet Temp"
threshold_c = 27.0
percent_per_degree_above = 2.0

[[sensor]]
name = "exhaust"
source = "ipmi"
sensor = "Exhaust Temp"
hard_ceiling_c = 75
curve = [[35, 20], [65, 80]]
"#;
        let cfg = Config::from_toml_str(cfg_str).unwrap();
        let bias = cfg.inlet_bias.as_ref().unwrap();
        assert_eq!(bias.sensor, "Inlet Temp");
        assert_eq!(bias.max_bias_pct, 30);
        match &cfg.sensors[0].spec {
            SensorSpec::Ipmi { sensor } => assert_eq!(sensor, "Exhaust Temp"),
            other => panic!("expected ipmi, got {:?}", other),
        }
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = r#"
nonsense_field = 42
[[sensor]]
name = "cpu"
source = "hwmon"
chip = "coretemp"
label = "Package id 0"
hard_ceiling_c = 90
curve = [[40, 20], [80, 80]]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }
}
