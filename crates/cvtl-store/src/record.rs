// FILE: crates/cvtl-store/src/record.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Encode stable bounded raw records
//   SCOPE: Record schema, line validation and setup metadata
//   DEPENDS: serde, serde_json, sha2
//   LINKS: M-CVTL-STORE
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Record - versioned raw envelope
//   Event - typed raw payload
//   A1StepV1 - optional thermal step evidence for the A1 scenario
//   encode_line - serialize one bounded line
//   decode_line - validate a complete line
//   Snapshot - applied configuration and reference bytes
// END_MODULE_MAP
//! Versioned raw records. The control path only serializes bounded complete lines.

use std::{
    borrow::Cow,
    io::{self, Write},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA: u16 = 1;
pub const MAX_RECORD_BYTES: usize = 16 * 1024;
pub const MAX_SAFETY_ISSUES: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "id")]
pub enum Correlation {
    Run,
    Tick(u64),
    Request(u64),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Measured,
    Missing,
    Invalid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    A,
    B,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PowerSample {
    pub channel: Channel,
    pub quality: Quality,
    pub current_a: Option<f64>,
    pub voltage_v: Option<f64>,
    pub target_w: Option<f64>,
    pub read_started_ns: Option<u64>,
    pub read_finished_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalPoint {
    Stage1,
    Stage2,
    Tf,
    Jacket,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct TemperatureSample {
    pub point: ThermalPoint,
    pub channel: Option<u8>,
    pub quality: Quality,
    pub temperature_k: Option<f64>,
    pub read_started_ns: Option<u64>,
    pub read_finished_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Demand {
    Observe,
    Replay,
    Power {
        requested_w: f64,
    },
    PowerProfile {
        requested_w: f64,
    },
    VirtualBody {
        requested_w: f64,
    },
    TemperatureHold {
        setpoint_k: f64,
        stage1_k: f64,
        stage2_k: f64,
        at_ns: u64,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Inputs<'a> {
    pub scheduled_ns: u64,
    pub measured_ns: u64,
    pub wall_time_s: f64,
    pub dt_ns: u64,
    pub power: [PowerSample; 2],
    pub temperatures: [TemperatureSample; 4],
    pub demand: Demand,
    pub source_connected: bool,
    pub thermometer_connected: bool,
    pub missed_tick: bool,
    pub allow_stale_once: bool,
    pub expect_tick: bool,
    pub queue_fault: bool,
    pub model_valid: bool,
    pub journal_ok: bool,
    pub cooling_phase: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a1_step_v1: Option<A1StepV1>,
}

/// A1 scenario extension. Absence preserves the original v1 Inputs encoding.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct A1StepV1 {
    pub interval_ns: u64,
    pub heater_energy_j: [f64; 2],
    pub held_tf_k: f64,
    pub held_tf_age_ns: u64,
    pub body_temperature_k: f64,
    pub change: Option<A1ChangeV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct A1ChangeV1 {
    pub scale: Option<f64>,
    pub bridge_w_per_k: Option<f64>,
    pub dh_event_j: Option<f64>,
    pub temperature_before_k: f64,
    pub temperature_after_k: f64,
}

impl A1StepV1 {
    fn valid(self) -> bool {
        self.interval_ns > 0
            && self.heater_energy_j.into_iter().all(nonnegative)
            && nonnegative(self.held_tf_k)
            && self.held_tf_k > 0.0
            && nonnegative(self.body_temperature_k)
            && self.body_temperature_k > 0.0
            && self.change.is_none_or(A1ChangeV1::valid)
    }
}

impl A1ChangeV1 {
    fn valid(self) -> bool {
        (self.scale.is_some() || self.bridge_w_per_k.is_some())
            && self.scale.is_none_or(|scale| scale > 0.0 && scale <= 1.0)
            && self.bridge_w_per_k.is_none_or(nonnegative)
            && self.scale.is_some() == self.dh_event_j.is_some()
            && self.dh_event_j.is_none_or(f64::is_finite)
            && nonnegative(self.temperature_before_k)
            && nonnegative(self.temperature_after_k)
    }
}

fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Issue<'a> {
    pub code: &'a str,
    pub channel: Option<Channel>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Command {
    On,
    Off,
    ZeroPreset,
    Drive { current_a: f64 },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Event<'a> {
    Inputs {
        #[serde(borrow)]
        frame: Box<Inputs<'a>>,
    },
    Parameters {
        mass_kg: Option<f64>,
        material: Option<Cow<'a, str>>,
        bridge: Option<Cow<'a, str>>,
        dh_event_j: Option<f64>,
        at_ns: u64,
    },
    Observation {
        at_ns: u64,
        state: &'a str,
        #[serde(borrow)]
        faults: Vec<Issue<'a>>,
        warnings: Vec<&'a str>,
    },
    Operator {
        action: &'a str,
        at_ns: u64,
    },
    Control {
        at_ns: u64,
        requested_w: Option<f64>,
        limited_w: Option<f64>,
        measured_w: Option<f64>,
        targets_w: Option<[f64; 2]>,
        equivalence: Option<&'a str>,
        slew_limited: bool,
        power_limited: bool,
        current_limited: bool,
        warning: Option<&'a str>,
        fault: Option<&'a str>,
    },
    CommandIntent {
        channel: Channel,
        command: Command,
        at_ns: u64,
    },
    Dispatch {
        source: &'a str,
        command: Cow<'a, str>,
        status: &'a str,
        /// Start of the transport attempt. Its status is known after the call returns.
        at_ns: u64,
    },
    Response {
        source: &'a str,
        command: Cow<'a, str>,
        bytes: Option<Cow<'a, [u8]>>,
        status: &'a str,
        at_ns: u64,
    },
    OutputEvidence {
        at_ns: u64,
        state: &'a str,
        fault: Option<&'a str>,
        fallback_requested: bool,
    },
}

impl Event<'_> {
    // events.ndjson is a view of selected original records, with their raw seq.
    pub(crate) fn in_operator_view(&self) -> bool {
        match self {
            Self::Observation {
                faults, warnings, ..
            } => !faults.is_empty() || !warnings.is_empty(),
            Self::Operator { .. } | Self::Parameters { .. } | Self::OutputEvidence { .. } => true,
            Self::Control { fault, warning, .. } => fault.is_some() || warning.is_some(),
            _ => false,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RecordError> {
        let finite = match self {
            Self::Inputs { frame } => {
                let demand = match frame.demand {
                    Demand::Observe | Demand::Replay => true,
                    Demand::Power { requested_w }
                    | Demand::PowerProfile { requested_w }
                    | Demand::VirtualBody { requested_w } => requested_w.is_finite(),
                    Demand::TemperatureHold {
                        setpoint_k,
                        stage1_k,
                        stage2_k,
                        ..
                    } => [setpoint_k, stage1_k, stage2_k]
                        .into_iter()
                        .all(f64::is_finite),
                };
                stable_tag(frame.cooling_phase)
                    && demand
                    && frame.a1_step_v1.is_none_or(|step| {
                        step.valid()
                            && (matches!(frame.demand, Demand::VirtualBody { .. })
                                || !frame.thermometer_connected
                                    && matches!(frame.demand, Demand::Observe))
                    })
                    && frame.wall_time_s.is_finite()
                    && frame.power.iter().all(|sample| {
                        [sample.current_a, sample.voltage_v, sample.target_w]
                            .into_iter()
                            .flatten()
                            .all(f64::is_finite)
                            && match sample.quality {
                                Quality::Measured => {
                                    sample.current_a.is_some() && sample.voltage_v.is_some()
                                }
                                Quality::Invalid | Quality::Missing => {
                                    sample.current_a.is_none() && sample.voltage_v.is_none()
                                }
                            }
                    })
                    && frame.temperatures.iter().all(|sample| {
                        sample.temperature_k.is_none_or(f64::is_finite)
                            && (sample.quality == Quality::Measured)
                                == sample.temperature_k.is_some()
                    })
            }
            Self::Parameters {
                mass_kg,
                dh_event_j,
                material,
                bridge,
                ..
            } => {
                check_text(material.as_deref())?;
                check_text(bridge.as_deref())?;
                [*mass_kg, *dh_event_j]
                    .into_iter()
                    .flatten()
                    .all(f64::is_finite)
            }
            Self::Observation {
                state,
                faults,
                warnings,
                ..
            } => {
                if faults.len() > MAX_SAFETY_ISSUES || warnings.len() > MAX_SAFETY_ISSUES {
                    return Err(RecordError::Size);
                }
                stable_tag(state)
                    && faults.iter().all(|issue| stable_tag(issue.code))
                    && warnings.iter().all(|tag| stable_tag(tag))
            }
            Self::Operator { action, .. } => stable_tag(action),
            Self::Control {
                requested_w,
                limited_w,
                measured_w,
                targets_w,
                equivalence,
                warning,
                fault,
                ..
            } => {
                [*requested_w, *limited_w, *measured_w]
                    .into_iter()
                    .flatten()
                    .all(f64::is_finite)
                    && targets_w.is_none_or(|values| values.into_iter().all(f64::is_finite))
                    && [*equivalence, *warning, *fault]
                        .into_iter()
                        .flatten()
                        .all(stable_tag)
            }
            Self::CommandIntent {
                command: Command::Drive { current_a },
                ..
            } => current_a.is_finite() && *current_a >= 0.0,
            Self::Dispatch {
                source,
                command,
                status,
                ..
            } => {
                check_text(Some(command))?;
                stable_tag(source) && stable_tag(status)
            }
            Self::Response {
                source,
                command,
                status,
                bytes,
                ..
            } => {
                check_text(Some(command))?;
                if bytes
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_RECORD_BYTES)
                {
                    return Err(RecordError::Size);
                }
                stable_tag(source) && stable_tag(status)
            }
            Self::OutputEvidence { state, fault, .. } => {
                stable_tag(state) && fault.is_none_or(stable_tag)
            }
            _ => true,
        };
        if !finite {
            return Err(RecordError::Format);
        }
        Ok(())
    }
}

fn check_text(text: Option<&str>) -> Result<(), RecordError> {
    if text.is_some_and(|value| value.len() > MAX_RECORD_BYTES) {
        return Err(RecordError::Size);
    }
    Ok(())
}

fn stable_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Record<'a> {
    pub schema: u16,
    pub run: &'a str,
    pub generation: u64,
    pub seq: u64,
    pub accepted_ns: u64,
    pub correlation: Correlation,
    #[serde(flatten, borrow)]
    pub event: Event<'a>,
}

impl<'a> Record<'a> {
    pub fn new(
        run: &'a str,
        generation: u64,
        seq: u64,
        accepted_ns: u64,
        correlation: Correlation,
        event: Event<'a>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            run,
            generation,
            seq,
            accepted_ns,
            correlation,
            event,
        }
    }

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != SCHEMA {
            return Err(RecordError::Schema);
        }
        if self.seq == 0 || self.generation == 0 || !valid_run(self.run) {
            return Err(RecordError::Format);
        }
        self.event.validate()
    }
}

pub(crate) fn valid_run(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 64
        && run
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordError {
    Schema,
    Format,
    Size,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Недопустимая запись: {self:?}")
    }
}
impl std::error::Error for RecordError {}

struct Line {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for Line {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > MAX_RECORD_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(io::Error::other("строка превышает 16 KiB"));
        }
        if buffer.len() > self.bytes.capacity() - self.bytes.len() {
            let limit = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(self.bytes.len() + buffer.len())
                .min(MAX_RECORD_BYTES);
            self.bytes.reserve_exact(limit - self.bytes.len());
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode_line(record: &Record<'_>) -> Result<Vec<u8>, RecordError> {
    record.validate()?;
    let mut line = Line {
        bytes: Vec::with_capacity(256),
        exceeded: false,
    };
    serde_json::to_writer(&mut line, record).map_err(|_| {
        if line.exceeded {
            RecordError::Size
        } else {
            RecordError::Format
        }
    })?;
    line.write_all(b"\n").map_err(|_| RecordError::Size)?;
    Ok(line.bytes)
}

pub fn decode_line(line: &[u8]) -> Result<Record<'_>, RecordError> {
    if line.len() > MAX_RECORD_BYTES {
        return Err(RecordError::Size);
    }
    let bytes = line.strip_suffix(b"\n").ok_or(RecordError::Format)?;
    if bytes.contains(&b'\n') {
        return Err(RecordError::Format);
    }
    // Inspect the envelope first so an unknown schema is not confused with bad
    // payload syntax when that version introduces different event variants.
    #[derive(Deserialize)]
    struct Version {
        schema: u16,
    }
    let version: Version = serde_json::from_slice(bytes).map_err(|_| RecordError::Format)?;
    if version.schema != SCHEMA {
        return Err(RecordError::Schema);
    }
    let record: Record<'_> = serde_json::from_slice(bytes).map_err(|_| RecordError::Format)?;
    record.validate()?;
    Ok(record)
}

pub struct Snapshot {
    pub config_yaml: String,
    pub scenario: String,
    pub references: Vec<(String, Vec<u8>)>,
}

impl Snapshot {
    pub fn metadata(&self, run: &str, generation: u64) -> Result<Vec<u8>, RecordError> {
        if !valid_run(run)
            || generation == 0
            || self.config_yaml.is_empty()
            || self.scenario.is_empty()
        {
            return Err(RecordError::Format);
        }
        let references = self
            .references
            .iter()
            .map(|(name, bytes)| {
                serde_json::json!({
                    "name": name,
                    "sha256": sha256(bytes),
                })
            })
            .collect::<Vec<_>>();
        let meta = serde_json::json!({
            "schema": SCHEMA,
            "run": run,
            "generation": generation,
            "software_version": env!("CARGO_PKG_VERSION"),
            "scenario": self.scenario,
            "config_sha256": sha256(self.config_yaml.as_bytes()),
            "references_status": if references.is_empty() { "not_used" } else { "embedded" },
            "references": references,
        });
        serde_json::to_vec_pretty(&meta).map_err(|_| RecordError::Format)
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
