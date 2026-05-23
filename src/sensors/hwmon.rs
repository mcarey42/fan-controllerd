//! Read temperatures from `/sys/class/hwmon`.
//!
//! For each `hwmonN` directory whose `name` file matches the configured
//! `chip`, walk all `tempN_input` files; for each, read the sibling
//! `tempN_label` and include the reading if the label matches the configured
//! `label`. Matching supports a single trailing `*` as a wildcard
//! (e.g. `label = "Package id *"`).
//!
//! Values in sysfs are integer millidegrees C — divide by 1000 to get C.

use std::fs;
use std::path::{Path, PathBuf};

use super::{SensorError, SensorReading};

const HWMON_ROOT: &str = "/sys/class/hwmon";

pub fn read_default(chip: &str, label: &str) -> Result<Vec<SensorReading>, SensorError> {
    read_from(Path::new(HWMON_ROOT), chip, label)
}

pub fn read_from(root: &Path, chip: &str, label: &str) -> Result<Vec<SensorReading>, SensorError> {
    let mut out = Vec::new();

    let entries = fs::read_dir(root)
        .map_err(|e| SensorError::Hwmon(format!("read_dir({}): {}", root.display(), e)))?;

    // Collect + sort so hwmon0, hwmon1, ... are visited in order — tests can
    // rely on stable ordering, and logs read more naturally.
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let name = match read_trim(&dir.join("name")) {
            Ok(n) => n,
            Err(_) => continue, // not all dirs under hwmon have a name file
        };
        if name != chip {
            continue;
        }

        // Find all temp*_input files in this hwmon dir.
        let mut input_files: Vec<PathBuf> = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| is_temp_input(p))
                .collect(),
            Err(_) => continue,
        };
        input_files.sort();

        for input in input_files {
            let lbl_path = label_path_for(&input);
            // Some hwmon entries (notably nvme's single-temp drivers) have no
            // label file. Default to "<chip>" or the file stem so the match
            // still works when the user configured label == chip name.
            let actual_label = read_trim(&lbl_path).unwrap_or_else(|_| name.clone());
            if !label_matches(&actual_label, label) {
                continue;
            }

            let raw = read_trim(&input)
                .map_err(|e| SensorError::Hwmon(format!("read({}): {}", input.display(), e)))?;
            let mc: i64 = raw.parse().map_err(|e| {
                SensorError::Hwmon(format!("parse temp {}: {}", input.display(), e))
            })?;
            let temp_c = mc as f32 / 1000.0;

            // Label like "hwmon3/Composite" so a Composite reading from drive
            // 3 is distinguishable in logs.
            let hwmon_id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("hwmon?")
                .to_string();
            out.push(SensorReading {
                label: format!("{}/{}", hwmon_id, actual_label),
                temp_c,
            });
        }
    }

    Ok(out)
}

fn read_trim(p: &Path) -> std::io::Result<String> {
    fs::read_to_string(p).map(|s| s.trim().to_string())
}

fn is_temp_input(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("temp") && name.ends_with("_input")
}

fn label_path_for(input: &Path) -> PathBuf {
    // /sys/.../tempN_input -> /sys/.../tempN_label
    let name = input.file_name().unwrap().to_string_lossy();
    let lbl = name.replace("_input", "_label");
    input.with_file_name(lbl)
}

/// Trailing `*` is a wildcard; otherwise exact match.
fn label_matches(actual: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        actual.starts_with(prefix)
    } else {
        actual == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    type FakeSensor<'a> = (&'a str, Option<&'a str>, i64);
    type FakeChip<'a> = (&'a str, &'a str, &'a [FakeSensor<'a>]);

    /// Build a fake /sys/class/hwmon tree under a tempdir.
    /// Returns the root path. `chips` is a list of (hwmon_dir, name, sensors)
    /// where each sensor is (tempN, label_or_none, mC_value).
    fn build_fake_hwmon(chips: &[FakeChip<'_>]) -> TempDir {
        let td = TempDir::new().unwrap();
        for (dirname, chip_name, sensors) in chips {
            let dir = td.path().join(dirname);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{chip_name}\n")).unwrap();
            for (temp, label, mc) in *sensors {
                fs::write(dir.join(format!("{temp}_input")), format!("{mc}\n")).unwrap();
                if let Some(l) = label {
                    fs::write(dir.join(format!("{temp}_label")), format!("{l}\n")).unwrap();
                }
            }
        }
        td
    }

    #[test]
    fn matches_exact_label() {
        let td = build_fake_hwmon(&[(
            "hwmon13",
            "coretemp",
            &[
                ("temp1", Some("Package id 0"), 38000),
                ("temp2", Some("Core 0"), 33000),
            ],
        )]);
        let r = read_from(td.path(), "coretemp", "Package id 0").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].temp_c, 38.0);
        assert_eq!(r[0].label, "hwmon13/Package id 0");
    }

    #[test]
    fn matches_wildcard() {
        let td = build_fake_hwmon(&[(
            "hwmon13",
            "coretemp",
            &[
                ("temp1", Some("Package id 0"), 38000),
                ("temp2", Some("Core 0"), 33000),
                ("temp3", Some("Core 1"), 32000),
            ],
        )]);
        let r = read_from(td.path(), "coretemp", "Core *").unwrap();
        assert_eq!(r.len(), 2);
        let temps: Vec<f32> = r.iter().map(|x| x.temp_c).collect();
        assert!(temps.contains(&33.0) && temps.contains(&32.0));
    }

    #[test]
    fn collects_across_multiple_hwmons() {
        // 3 fake NVMe drives; "Composite" label on each.
        let td = build_fake_hwmon(&[
            ("hwmon0", "nvme", &[("temp1", Some("Composite"), 27850)]),
            ("hwmon1", "nvme", &[("temp1", Some("Composite"), 30850)]),
            ("hwmon2", "nvme", &[("temp1", Some("Composite"), 33850)]),
            (
                "hwmon3",
                "power_meter",
                &[("temp1", Some("Composite"), 50000)],
            ), // wrong chip
        ]);
        let r = read_from(td.path(), "nvme", "Composite").unwrap();
        assert_eq!(r.len(), 3);
        let mut temps: Vec<f32> = r.iter().map(|x| x.temp_c).collect();
        temps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(temps, vec![27.85, 30.85, 33.85]);
    }

    #[test]
    fn missing_label_file_falls_back_to_chip_name() {
        // Old nvme drivers expose just temp1_input with no label file.
        let td = build_fake_hwmon(&[("hwmon0", "nvme", &[("temp1", None, 40000)])]);
        let r = read_from(td.path(), "nvme", "nvme").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].temp_c, 40.0);
    }

    #[test]
    fn no_match_returns_empty() {
        let td = build_fake_hwmon(&[(
            "hwmon0",
            "coretemp",
            &[("temp1", Some("Package id 0"), 38000)],
        )]);
        let r = read_from(td.path(), "nvme", "Composite").unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn ignores_non_temp_files() {
        let td = TempDir::new().unwrap();
        let d = td.path().join("hwmon0");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), "coretemp\n").unwrap();
        fs::write(d.join("temp1_input"), "38000\n").unwrap();
        fs::write(d.join("temp1_label"), "Package id 0\n").unwrap();
        fs::write(d.join("in1_input"), "1000\n").unwrap(); // voltage, must be ignored
        fs::write(d.join("fan1_input"), "1500\n").unwrap();
        let r = read_from(td.path(), "coretemp", "Package id 0").unwrap();
        assert_eq!(r.len(), 1);
    }
}
