//! Shared configuration and the Phase A startup gate.

use serde::Deserialize;
use std::{error::Error, fmt, fs, path::Path};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    schema_version: u32,
    hardware_enabled: bool,
    // Reserved schema fields are parsed now and consumed by later milestones.
    #[allow(dead_code)]
    run_permission: RunPermission,
    #[allow(dead_code)]
    thermometry: Thermometry,
    sources: Sources,
    safety: Safety,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunPermission {
    Unapproved,
    Approved,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Thermometry {
    #[allow(dead_code)]
    transport: ThermometryTransport,
    #[allow(dead_code)]
    channels: ThermometryChannels,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ThermometryTransport {
    GpibExistingAdapter,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThermometryChannels {
    #[allow(dead_code)]
    stage2: Option<String>,
    #[allow(dead_code)]
    tf: Option<String>,
    #[allow(dead_code)]
    stage1: Option<String>,
    #[allow(dead_code)]
    jacket: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    keithley: Keithley,
    #[allow(dead_code)]
    maisheng_1: UnverifiedSource,
    #[allow(dead_code)]
    maisheng_2: UnverifiedSource,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Keithley {
    #[allow(dead_code)]
    resource: Option<String>,
    #[allow(dead_code)]
    channels: SourceChannels,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceChannels {
    #[serde(rename = "A")]
    a: SourceChannel,
    #[serde(rename = "B")]
    b: SourceChannel,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceChannel {
    #[allow(dead_code)]
    heater_id: Option<String>,
    heater_limit_w: Option<f64>,
    #[allow(dead_code)]
    enabled_for_run: bool,
    k_v: f64,
    k_i: f64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnverifiedSource {
    #[allow(dead_code)]
    protocol_verified: bool,
    #[allow(dead_code)]
    enabled_for_run: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Safety {
    head_envelope: Option<yaml_serde::Value>,
    total_power_limit_w: Option<f64>,
    slew_up_w_per_s: Option<f64>,
    slew_down_w_per_s: Option<f64>,
    age_max_s: Option<f64>,
    #[allow(dead_code)]
    independent_cutoff_verified: bool,
}

#[derive(Debug)]
pub struct ConfigError(String);
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for ConfigError {}

impl Config {
    /// Parses and validates a Phase A configuration. Hardware is always blocked.
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        let config: Self = yaml_serde::from_str(yaml)
            .map_err(|e| ConfigError(format!("Ошибка конфигурации YAML: {e}")))?;
        if config.schema_version != 1 {
            return Err(ConfigError("Неподдерживаемая версия схемы".into()));
        }
        if config.hardware_enabled {
            return Err(ConfigError(
                "Доступ к оборудованию запрещён в фазе A".into(),
            ));
        }
        if config.safety.head_envelope.is_some() {
            return Err(ConfigError("head_envelope пока не поддерживается".into()));
        }
        for channel in [
            &config.sources.keithley.channels.a,
            &config.sources.keithley.channels.b,
        ] {
            if !(channel.k_v.is_finite()
                && channel.k_i.is_finite()
                && (0.0..=1.0).contains(&channel.k_v)
                && (0.0..=1.0).contains(&channel.k_i))
            {
                return Err(ConfigError(
                    "Коэффициенты каналов должны быть конечными и лежать в пределах 0..=1".into(),
                ));
            }
        }
        if config
            .safety
            .age_max_s
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
        {
            return Err(ConfigError(
                "age_max_s должен быть конечным положительным числом".into(),
            ));
        }
        for value in [
            config.sources.keithley.channels.a.heater_limit_w,
            config.sources.keithley.channels.b.heater_limit_w,
            config.safety.total_power_limit_w,
            config.safety.slew_up_w_per_s,
            config.safety.slew_down_w_per_s,
        ]
        .into_iter()
        .flatten()
        {
            if !value.is_finite() || value <= 0.0 {
                return Err(ConfigError(
                    "Заданные пределы безопасности должны быть конечными положительными числами"
                        .into(),
                ));
            }
        }
        Ok(config)
    }
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let yaml = fs::read_to_string(path)
            .map_err(|e| ConfigError(format!("Не удалось прочитать конфигурацию: {e}")))?;
        Self::from_yaml(&yaml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn configurations(dir: &Path, result: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("config directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                configurations(&path, result);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yaml" | "yml")
            ) {
                result.push(path);
            }
        }
    }
    #[test]
    fn all_shipped_configs_are_safe_defaults() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let mut files = Vec::new();
        configurations(&dir, &mut files);
        assert!(!files.is_empty(), "no shipped YAML configurations found");
        for file in files {
            let c = Config::from_file(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            assert!(!c.hardware_enabled && matches!(c.run_permission, RunPermission::Unapproved));
            assert!(
                c.safety.head_envelope.is_none()
                    && c.safety.total_power_limit_w.is_none()
                    && c.safety.slew_up_w_per_s.is_none()
                    && c.safety.slew_down_w_per_s.is_none()
            );
            assert!(c.safety.age_max_s.is_none());
            assert!(!c.safety.independent_cutoff_verified && c.sources.keithley.resource.is_none());
            for channel in [
                &c.sources.keithley.channels.a,
                &c.sources.keithley.channels.b,
            ] {
                assert!(
                    channel.heater_id.is_none()
                        && channel.heater_limit_w.is_none()
                        && !channel.enabled_for_run
                );
            }
            for source in [&c.sources.maisheng_1, &c.sources.maisheng_2] {
                assert!(!source.protocol_verified && !source.enabled_for_run);
            }
            let t = &c.thermometry.channels;
            assert!(
                t.stage1.is_none() && t.stage2.is_none() && t.tf.is_none() && t.jacket.is_none()
            );
        }
    }
    #[test]
    fn startup_rejects_hardware_and_invalid_configuration() {
        let yaml = include_str!("../../../config/simulator.yaml");
        assert!(Config::from_yaml(yaml).is_ok());
        assert!(Config::from_yaml(
            &yaml.replace("hardware_enabled: false", "hardware_enabled: true")
        )
        .is_err());
        assert!(
            Config::from_yaml(&yaml.replace("schema_version: 1", "schema_version: 2")).is_err()
        );
        assert!(Config::from_yaml(
            &yaml.replace("hardware_enabled: false", "hardware_enabld: false")
        )
        .is_err());
        assert!(Config::from_yaml(&yaml.replace("age_max_s: null", "age_max_s: .nan")).is_err());
        assert!(
            Config::from_yaml(&yaml.replace("head_envelope: null", "head_envelope: {foo: 1}"))
                .is_err()
        );
    }
}
