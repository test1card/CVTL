// FILE: crates/cvtl-cli/src/replay/profile.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Load and validate the recorded v1 profile
//   SCOPE: Verify identity and digests, construct existing controller and supervisor
//   DEPENDS: M-CVTL-CORE, M-CVTL-CONTROL, M-CVTL-SAFETY, M-CVTL-STORE
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   load - validate and load a replay profile
//   open_file - open a local regular input file
// END_MODULE_MAP
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::Duration,
};

use cvtl_control::{allocation::Allocator, pi::PiConfig, ControlConfig, Controller};
use cvtl_core::{
    units::{
        Amperes, Kelvin, KelvinPerSecond, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond,
    },
    Config,
};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_safety::{
    monitor::{HeadProfile, ProtectionProfile},
    supervisor::Supervisor,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::quantity;

const MAX_HEADER: u64 = 64 * 1024;
const PROFILE: &str = "m6_recording_v1";

#[derive(Deserialize)]
pub(super) struct Meta {
    pub run: String,
    pub generation: u64,
    schema: u16,
    software_version: String,
    scenario: String,
    config_sha256: String,
    references_status: String,
    references: Vec<Reference>,
}
#[derive(Deserialize)]
struct Reference {
    name: String,
    sha256: String,
}
#[derive(Deserialize)]
struct Snapshot {
    hardware_enabled: bool,
    phase_a_config: yaml_serde::Value,
    test_only: TestOnly,
}
#[derive(Deserialize)]
struct TestOnly {
    scenario: String,
    temperature_channels: TemperatureChannels,
    control: Control,
    pi: Pi,
    heater: Heater,
    safety: Safety,
    head: Head,
}
#[derive(Deserialize)]
struct TemperatureChannels {
    stage1: u8,
    stage2: u8,
}
#[derive(Deserialize)]
struct Control {
    kp: f64,
    ki_per_s: f64,
    total_limit_w: f64,
    max_sample_age_s: f64,
    margin: f64,
    slew_w_per_s: f64,
}
#[derive(Deserialize)]
struct Pi {
    integral_limit_w: f64,
    r_initial_ohm: f64,
    r_min_ohm: f64,
    r_max_ohm: f64,
    r_tau_s: f64,
    min_current_a: f64,
    quantum: Option<yaml_serde::Value>,
    derivative: Option<yaml_serde::Value>,
}
#[derive(Deserialize)]
struct Heater {
    loop_ohm: f64,
    limit_w: f64,
}
#[derive(Deserialize)]
struct Safety {
    profile: String,
    max_current_a: [f64; 2],
    max_voltage_v: [f64; 2],
    max_branch_power_w: [f64; 2],
    max_total_power_w: f64,
    phase_power_limits_w: [f64; 3],
    max_temperature_age_ms: u64,
    max_tick_gap_ms: u64,
    tracking_tolerance_w: f64,
    tracking_duration_ms: u64,
    wall_jump_tolerance_s: f64,
}
#[derive(Deserialize)]
struct Head {
    inversion_warning_k: f64,
    inversion_derate_k: f64,
    inversion_trip_k: f64,
    temperature_warning_k: f64,
    temperature_derate_k: f64,
    temperature_trip_k: f64,
    min_temperature_k: f64,
    derated_power_w: f64,
    max_rate_k_per_s: f64,
}

pub(super) struct Profile {
    pub meta: Meta,
    pub controller: Controller,
    pub supervisor: Supervisor,
}

pub(super) fn load(run: &Path) -> Result<Profile, String> {
    if !fs::symlink_metadata(run).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Err(format!(
            "Ожидается локальный каталог прогона: {}",
            run.display()
        ));
    }
    let bytes = header(&run.join("meta.json"))?;
    let meta: Meta =
        serde_json::from_slice(&bytes).map_err(|e| format!("Ошибка meta.json: {e}"))?;
    if meta.schema != cvtl_store::record::SCHEMA
        || meta.software_version != env!("CARGO_PKG_VERSION")
        || meta.scenario != "test_only_m6_recording_v1"
        || !safe_name(&meta.run)
        || meta.generation == 0
    {
        return Err("Несовместимая схема, профиль, версия или идентификатор прогона".into());
    }
    let config = header(&run.join("config.snapshot.yaml"))?;
    check_hash(
        "config.snapshot.yaml",
        &meta.config_sha256,
        Sha256::digest(&config),
    )?;
    verify_references(run, &meta)?;
    let snapshot: Snapshot =
        yaml_serde::from_slice(&config).map_err(|e| format!("Ошибка снимка YAML: {e}"))?;
    if snapshot.hardware_enabled {
        return Err("Снимок включает доступ к оборудованию".into());
    }
    let phase_a = yaml_serde::to_string(&snapshot.phase_a_config)
        .map_err(|e| format!("Ошибка phase_a_config: {e}"))?;
    Config::from_yaml(&phase_a).map_err(|e| e.to_string())?;
    let test = snapshot.test_only;
    if test.scenario != PROFILE
        || test.safety.profile != "m5_test_only_example"
        || test.pi.quantum.is_some()
        || test.pi.derivative.is_some()
    {
        return Err("Неподдерживаемый профиль параметров REPLAY v1".into());
    }
    let controller = test.controller()?;
    let supervisor = test.supervisor()?;
    Ok(Profile {
        meta,
        controller,
        supervisor,
    })
}

pub(super) fn open_file(path: &Path) -> Result<File, String> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err(format!(
            "Ожидается обычный локальный файл: {}",
            path.display()
        ));
    }
    File::open(path).map_err(|e| format!("Не удалось прочитать {}: {e}", path.display()))
}

fn header(path: &Path) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    open_file(path)?
        .take(MAX_HEADER + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("Не удалось прочитать {}: {e}", path.display()))?;
    if data.len() as u64 > MAX_HEADER {
        return Err(format!("Слишком большой заголовок: {}", path.display()));
    }
    Ok(data)
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn check_hash(name: &str, expected: &str, actual: impl std::fmt::LowerHex) -> Result<(), String> {
    if expected != format!("{actual:x}") {
        return Err(format!("Не совпадает SHA-256: {name}"));
    }
    Ok(())
}

fn verify_references(run: &Path, meta: &Meta) -> Result<(), String> {
    match (meta.references_status.as_str(), meta.references.is_empty()) {
        ("not_used", true) => return Ok(()),
        ("embedded", false) => {}
        _ => return Err("Некорректное описание локальных справочников".into()),
    }
    let dir = run.join("references");
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Err("Каталог локальных справочников отсутствует или несовместим".into());
    }
    let mut names = HashSet::new();
    for reference in &meta.references {
        if !safe_name(&reference.name) || !names.insert(reference.name.as_str()) {
            return Err("Недопустимое или повторное имя справочника".into());
        }
        let mut file = open_file(&dir.join(&reference.name))?;
        let mut hash = Sha256::new();
        io::copy(&mut file, &mut hash).map_err(|e| format!("Ошибка чтения справочника: {e}"))?;
        check_hash(&reference.name, &reference.sha256, hash.finalize())?;
    }
    Ok(())
}

fn quantities<T: Copy, const N: usize>(
    values: [f64; N],
    convert: impl Fn(f64) -> Option<T>,
    field: &str,
) -> Result<[T; N], String> {
    let mut result = [quantity(convert(values[0]), field)?; N];
    for (out, value) in result.iter_mut().zip(values) {
        *out = quantity(convert(value), field)?;
    }
    Ok(result)
}

impl TestOnly {
    fn controller(&self) -> Result<Controller, String> {
        let pi = &self.pi;
        let c = &self.control;
        let range = ResistanceRange::new(
            quantity(Ohms::new(pi.r_min_ohm), "r_min_ohm")?,
            quantity(Ohms::new(pi.r_max_ohm), "r_max_ohm")?,
        )
        .map_err(|_| "Некорректный диапазон сопротивления снимка")?;
        let config = ControlConfig {
            pi: [PiConfig {
                kp: c.kp,
                ki: quantity(PerSecond::new(c.ki_per_s), "ki_per_s")?,
                integral_limit: quantity(Watts::new(pi.integral_limit_w), "integral_limit_w")?,
                resistance_initial: quantity(Ohms::new(pi.r_initial_ohm), "r_initial_ohm")?,
                resistance_min: range.bounds().0,
                resistance_max: range.bounds().1,
                resistance_tau: quantity(Seconds::new(pi.r_tau_s), "r_tau_s")?,
                min_current: quantity(Amperes::new(pi.min_current_a), "min_current_a")?,
                current_quantum: None,
                derivative: None,
            }; 2],
            branches: [ChannelSpec {
                resistance: range,
                loop_resistance: quantity(Ohms::new(self.heater.loop_ohm), "loop_ohm")?,
                heater_limit: Some(quantity(Watts::new(self.heater.limit_w), "limit_w")?),
            }; 2],
            margins: Margins::new(c.margin, c.margin).map_err(|_| "Некорректный запас снимка")?,
            total_limit: quantity(Watts::new(c.total_limit_w), "total_limit_w")?,
            max_sample_age: quantity(Seconds::new(c.max_sample_age_s), "max_sample_age_s")?,
            temperature_hold: None,
        };
        let slew = quantity(WattsPerSecond::new(c.slew_w_per_s), "slew_w_per_s")?;
        let allocator = Allocator::new(None, slew, slew, [slew; 2], [slew; 2])
            .map_err(|_| "Некорректный ограничитель скорости снимка")?;
        Controller::new(config, allocator).map_err(|_| "Некорректный профиль M4".into())
    }

    fn supervisor(&self) -> Result<Supervisor, String> {
        let s = &self.safety;
        let h = &self.head;
        let profile = ProtectionProfile {
            max_current_a: quantities(s.max_current_a, Amperes::new, "max_current_a")?,
            max_voltage_v: quantities(s.max_voltage_v, Volts::new, "max_voltage_v")?,
            max_branch_power_w: quantities(s.max_branch_power_w, Watts::new, "max_branch_power_w")?,
            max_total_power_w: quantity(Watts::new(s.max_total_power_w), "max_total_power_w")?,
            phase_power_limits_w: quantities(
                s.phase_power_limits_w,
                Watts::new,
                "phase_power_limits_w",
            )?,
            max_temperature_age: Duration::from_millis(s.max_temperature_age_ms),
            max_tick_gap: Duration::from_millis(s.max_tick_gap_ms),
            tracking_tolerance_w: quantity(
                Watts::new(s.tracking_tolerance_w),
                "tracking_tolerance_w",
            )?,
            tracking_duration: Duration::from_millis(s.tracking_duration_ms),
            wall_jump_tolerance_s: quantity(
                Seconds::new(s.wall_jump_tolerance_s),
                "wall_jump_tolerance_s",
            )?,
            stage1_channel: self.temperature_channels.stage1,
            stage2_channel: self.temperature_channels.stage2,
            head: HeadProfile {
                inversion_warning_k: quantity(
                    Kelvin::new(h.inversion_warning_k),
                    "inversion_warning_k",
                )?,
                inversion_derate_k: quantity(
                    Kelvin::new(h.inversion_derate_k),
                    "inversion_derate_k",
                )?,
                inversion_trip_k: quantity(Kelvin::new(h.inversion_trip_k), "inversion_trip_k")?,
                temperature_warning_k: quantity(
                    Kelvin::new(h.temperature_warning_k),
                    "temperature_warning_k",
                )?,
                temperature_derate_k: quantity(
                    Kelvin::new(h.temperature_derate_k),
                    "temperature_derate_k",
                )?,
                temperature_trip_k: quantity(
                    Kelvin::new(h.temperature_trip_k),
                    "temperature_trip_k",
                )?,
                min_temperature_k: quantity(Kelvin::new(h.min_temperature_k), "min_temperature_k")?,
                derated_power_w: quantity(Watts::new(h.derated_power_w), "derated_power_w")?,
                max_rate_k_per_s: quantity(
                    KelvinPerSecond::new(h.max_rate_k_per_s),
                    "max_rate_k_per_s",
                )?,
            },
        };
        Supervisor::new(profile).map_err(|_| "Некорректный профиль M5".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cvtl_sim::recording::RecordingLoop;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "cvtl-replay-profile-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, snapshot: &cvtl_store::record::Snapshot) {
            std::fs::write(self.0.join("config.snapshot.yaml"), &snapshot.config_yaml).unwrap();
            std::fs::write(
                self.0.join("meta.json"),
                snapshot.metadata("run-v1", 1).unwrap(),
            )
            .unwrap();
            if !snapshot.references.is_empty() {
                std::fs::create_dir_all(self.0.join("references")).unwrap();
                for (name, bytes) in &snapshot.references {
                    std::fs::write(self.0.join("references").join(name), bytes).unwrap();
                }
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_real_v1_snapshot_and_checks_reference_hashes() {
        let dir = Fixture::new();
        let mut snapshot = RecordingLoop::snapshot();
        snapshot
            .references
            .push(("table".into(), b"table-data".to_vec()));
        dir.write(&snapshot);
        let profile = load(&dir.0).unwrap();
        assert_eq!(profile.meta.run, "run-v1");
        assert_eq!(profile.meta.generation, 1);
        std::fs::write(dir.0.join("references/table"), b"changed").unwrap();
        assert!(load(&dir.0).err().unwrap().contains("SHA-256"));
    }

    #[test]
    fn rejects_mutated_snapshot_before_using_its_values() {
        let dir = Fixture::new();
        let mut snapshot = RecordingLoop::snapshot();
        dir.write(&snapshot);
        snapshot.config_yaml.push(' ');
        std::fs::write(dir.0.join("config.snapshot.yaml"), snapshot.config_yaml).unwrap();
        assert!(load(&dir.0).err().unwrap().contains("SHA-256"));
    }

    #[test]
    fn rejects_unknown_profile_and_hardware_even_with_matching_hash() {
        for (from, to) in [
            ("scenario: m6_recording_v1", "scenario: unknown"),
            ("hardware_enabled: false", "hardware_enabled: true"),
            ("quantum: null", "quantum: 0"),
        ] {
            let dir = Fixture::new();
            let mut snapshot = RecordingLoop::snapshot();
            snapshot.config_yaml = snapshot.config_yaml.replacen(from, to, 1);
            dir.write(&snapshot);
            assert!(load(&dir.0).is_err(), "accepted {to}");
        }
    }
}
