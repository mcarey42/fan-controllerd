//! Sensor backends — read temperatures from hwmon (sysfs) and IPMI SDR.
//!
//! A single configured `[[sensor]]` entry may expand to multiple physical
//! readings (e.g. `chip = "nvme", label = "Composite"` matches all 8 NVMe
//! drives). The per-sensor demand is the max temp across those readings.

pub mod hwmon;
pub mod ipmi;

use crate::config::SensorSpec;

#[derive(Debug, Clone)]
pub struct SensorReading {
    /// Human-readable identifier for logs (e.g. "hwmon0/Composite",
    /// "Exhaust Temp"). Not used for control logic.
    pub label: String,
    pub temp_c: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum SensorError {
    #[error("hwmon error: {0}")]
    Hwmon(String),
    #[error("ipmi error: {0}")]
    Ipmi(String),
    #[error("no readings for sensor spec {0:?}")]
    NoMatches(SensorSpec),
}

/// Read all temperatures matching one configured sensor spec.
///
/// The IPMI cache must be refreshed once per tick before calling this for
/// any IPMI specs — see `ipmi::IpmiCache::refresh`.
pub fn read(spec: &SensorSpec, ipmi: &ipmi::IpmiCache) -> Result<Vec<SensorReading>, SensorError> {
    match spec {
        SensorSpec::Hwmon { chip, label } => {
            let readings = hwmon::read_default(chip, label)?;
            if readings.is_empty() {
                Err(SensorError::NoMatches(spec.clone()))
            } else {
                Ok(readings)
            }
        }
        SensorSpec::Ipmi { sensor } => match ipmi.get(sensor) {
            Some(t) => Ok(vec![SensorReading {
                label: sensor.clone(),
                temp_c: t,
            }]),
            None => Err(SensorError::NoMatches(spec.clone())),
        },
    }
}

/// Reduce multiple readings from a single spec to one temperature (hottest
/// wins). Returns None if the slice is empty.
pub fn hottest(readings: &[SensorReading]) -> Option<&SensorReading> {
    readings.iter().max_by(|a, b| {
        a.temp_c
            .partial_cmp(&b.temp_c)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}
