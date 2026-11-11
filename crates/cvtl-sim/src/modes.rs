// FILE: crates/cvtl-sim/src/modes.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Share test-only mode settings and demand calculation with replay
//   SCOPE: Mode snapshots, ordinary demands and virtual body evolution
//   DEPENDS: M-CVTL-CORE, M-CVTL-CONTROL, M-CVTL-MATERIALS, M-CVTL-PHYSICS, M-CVTL-SAFETY, M-CVTL-STORE
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   ModeSetup - validated operator selection and ordinary tick demand
//   ModeSnapshot - reproducible synthetic scenario settings
//   VirtualBody - integrate and reconfigure the test-only body at a selected port
//   Compensation - evaluate causal fixture compensation from accepted readings
//   control_demand - convert recorded units into M4 inputs
// END_MODULE_MAP

use std::time::Duration;

use cvtl_control::{Demand, TemperatureHoldConfig};
use cvtl_core::units::{
    Joules, JoulesPerKelvin, Kelvin, KelvinPerSecond, Kilograms, Seconds, Watts, WattsPerKelvin,
    WattsPerKelvinSecond,
};
use cvtl_materials::nist_heat_capacity_from_catalog;
use cvtl_physics::{
    fixture_heat_capacity, request,
    scenario::{BodySetup, BridgeSetup},
    Body, Bridge, EmulationMode, InventoryPart, PortSample,
};
use cvtl_safety::monitor::ProtectionProfile;
use cvtl_store::record::{
    Demand as RawDemand, ModeBodyChangeV1, ModeBodyStepV1, ModeChangeOutcomeV1,
    ModeCompensationIssue, ModeCompensationV1, Quality, TemperatureSample, ThermalPoint,
};
use serde::{Deserialize, Serialize};

use crate::{
    a1::{
        held_age, A1PreviewPoint, CAPACITY_J_PER_K, DEFAULT_REMOVAL_W, HEAD_K, HEAD_LINKS_W_PER_K,
        JACKET_K,
    },
    head::{HeadParameters, HeadTemperatures, SyntheticHead},
};

pub use crate::a1::CATALOG;

pub const PROFILE: &str = "test_only_operator_modes_v1";
pub const PERIOD: Duration = Duration::from_millis(100);
const PREVIEW_STEPS: u32 = 30;
const NS_PER_MS: u64 = 1_000_000;
const HOLD_GAINS: HoldSetup = HoldSetup {
    kp_w_per_k: 2.0,
    ki_w_per_k_s: 0.5,
};

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModeSetup {
    Observe,
    Constant {
        power_w: f64,
    },
    PowerProfile {
        points: Vec<PowerPoint>,
    },
    TemperatureHold {
        setpoint_k: f64,
    },
    VirtualBody {
        #[serde(default)]
        port: BodyPort,
        #[serde(default)]
        body_heat_w: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compensation: Option<CompensationSetup>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PowerPoint {
    pub elapsed_ms: u64,
    pub power_w: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyPort {
    Stage2,
    #[default]
    Tf,
}

impl BodyPort {
    fn channel(self) -> (ThermalPoint, u8) {
        match self {
            Self::Stage2 => (ThermalPoint::Stage2, 2),
            Self::Tf => (ThermalPoint::Tf, 3),
        }
    }

    fn temperature(self, head: HeadTemperatures) -> Kelvin {
        match self {
            Self::Stage2 => head.stage2,
            Self::Tf => head.tf,
        }
    }

    fn sample(
        self,
        temperatures: &[TemperatureSample; 4],
        measured_ns: u64,
        max_age: Duration,
    ) -> Option<HeldPort> {
        let (point, channel) = self.channel();
        let (temperature_k, read_started_ns) = stage_reading(
            temperatures[usize::from(channel - 1)],
            point,
            channel,
            measured_ns,
        )?;
        held_age(read_started_ns, measured_ns, max_age)?;
        (temperature_k > 0.0).then_some(HeldPort {
            temperature: Kelvin::new(temperature_k)?,
            read_started_ns,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationSetup {
    pub fixture_parasitic_w: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixture: Vec<FixturePartSetup>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixturePartSetup {
    pub material: String,
    pub mass_kg: f64,
}

impl ModeSetup {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Constant { .. } => "constant",
            Self::PowerProfile { .. } => "power_profile",
            Self::TemperatureHold { .. } => "temperature_hold",
            Self::VirtualBody { .. } => "virtual_body",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let valid = match self {
            Self::Observe => true,
            Self::Constant { power_w } => nonnegative(*power_w),
            Self::PowerProfile { points } => {
                points.first().is_some_and(|point| point.elapsed_ms == 0)
                    && points.iter().all(|point| {
                        point.elapsed_ms.checked_mul(NS_PER_MS).is_some()
                            && nonnegative(point.power_w)
                    })
                    && points
                        .windows(2)
                        .all(|pair| pair[0].elapsed_ms < pair[1].elapsed_ms)
            }
            Self::TemperatureHold { setpoint_k } => setpoint_k.is_finite() && *setpoint_k > 0.0,
            Self::VirtualBody {
                body_heat_w,
                compensation,
                ..
            } => {
                nonnegative(*body_heat_w)
                    && compensation.as_ref().is_none_or(|setup| {
                        nonnegative(setup.fixture_parasitic_w)
                            && !setup.fixture.is_empty()
                            && setup.fixture.iter().all(|part| {
                                !part.material.is_empty()
                                    && part.mass_kg.is_finite()
                                    && part.mass_kg > 0.0
                            })
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(format!("Недопустимые параметры режима {}", self.kind()))
        }
    }

    pub fn power_at(&self, elapsed_ns: u64) -> Result<Watts, String> {
        self.validate()?;
        let power_w = match self {
            Self::Constant { power_w } => *power_w,
            Self::PowerProfile { points } => interpolate_power(points, elapsed_ns),
            _ => return Err("Режим не задаёт таблицу или постоянную мощность".into()),
        };
        Watts::new(power_w).ok_or_else(|| "Мощность запроса должна быть конечной".into())
    }

    pub fn ordinary_demand(
        &self,
        start_ns: Option<u64>,
        measured_ns: u64,
        temperatures: &[TemperatureSample; 4],
    ) -> Result<RawDemand, String> {
        let Some(start_ns) = start_ns else {
            return Ok(RawDemand::Observe);
        };
        let elapsed_ns = measured_ns
            .checked_sub(start_ns)
            .ok_or("Измерение предшествует принятому пуску")?;
        match self {
            Self::Observe => Ok(RawDemand::Observe),
            Self::Constant { .. } => Ok(RawDemand::Power {
                requested_w: self.power_at(elapsed_ns)?.get(),
            }),
            Self::PowerProfile { .. } => Ok(RawDemand::PowerProfile {
                requested_w: self.power_at(elapsed_ns)?.get(),
            }),
            Self::TemperatureHold { setpoint_k } => {
                self.validate()?;
                let first = stage_reading(temperatures[0], ThermalPoint::Stage1, 1, measured_ns);
                let second = stage_reading(temperatures[1], ThermalPoint::Stage2, 2, measured_ns);
                let (Some((stage1_k, stage1_ns)), Some((stage2_k, stage2_ns))) = (first, second)
                else {
                    return Ok(RawDemand::Observe);
                };
                // M5 owns freshness and inversion decisions for this same pair.
                Ok(RawDemand::TemperatureHold {
                    setpoint_k: *setpoint_k,
                    stage1_k,
                    stage2_k,
                    at_ns: stage1_ns.min(stage2_ns),
                })
            }
            Self::VirtualBody { .. } => {
                Err("Запрос виртуального тела требует теплового шага".into())
            }
        }
    }

    pub fn temperature_age(&self) -> Duration {
        if matches!(self, Self::TemperatureHold { .. }) {
            Duration::from_millis(150)
        } else {
            ProtectionProfile::test_only_example().max_temperature_age
        }
    }

    pub fn hold_config(&self) -> Option<TemperatureHoldConfig> {
        matches!(self, Self::TemperatureHold { .. }).then(|| TemperatureHoldConfig {
            kp: WattsPerKelvin::new(HOLD_GAINS.kp_w_per_k).unwrap(),
            ki: WattsPerKelvinSecond::new(HOLD_GAINS.ki_w_per_k_s).unwrap(),
        })
    }
}

fn interpolate_power(points: &[PowerPoint], elapsed_ns: u64) -> f64 {
    let upper = points.partition_point(|point| point.elapsed_ms * NS_PER_MS <= elapsed_ns);
    let left = points[upper - 1];
    let left_ns = left.elapsed_ms * NS_PER_MS;
    let Some(right) = points.get(upper).filter(|_| elapsed_ns != left_ns) else {
        return left.power_w;
    };
    let span_ns = right.elapsed_ms * NS_PER_MS - left_ns;
    let fraction = (elapsed_ns - left_ns) as f64 / span_ns as f64;
    let power = left.power_w + fraction * (right.power_w - left.power_w);
    power.clamp(
        left.power_w.min(right.power_w),
        left.power_w.max(right.power_w),
    )
}

fn stage_reading(
    sample: TemperatureSample,
    point: ThermalPoint,
    channel: u8,
    measured_ns: u64,
) -> Option<(f64, u64)> {
    let started_ns = sample.read_started_ns?;
    let finished_ns = sample.read_finished_ns?;
    let temperature_k = sample.temperature_k?;
    (sample.point == point
        && sample.channel == Some(channel)
        && sample.quality == Quality::Measured
        && started_ns <= finished_ns
        && finished_ns <= measured_ns
        && temperature_k.is_finite())
    .then_some((temperature_k, started_ns))
}

fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

pub fn control_demand(raw: RawDemand) -> Result<Demand, String> {
    let watts = |value| Watts::new(value).ok_or("Неконечная мощность запроса");
    let kelvin = |value| Kelvin::new(value).ok_or("Неконечная температура запроса");
    Ok(match raw {
        RawDemand::Observe => Demand::Observe,
        RawDemand::Replay => Demand::Replay,
        RawDemand::Power { requested_w } => Demand::Power(watts(requested_w)?),
        RawDemand::PowerProfile { requested_w } => Demand::PowerProfile(watts(requested_w)?),
        RawDemand::VirtualBody { requested_w } => Demand::VirtualBody(watts(requested_w)?),
        RawDemand::TemperatureHold {
            setpoint_k,
            stage1_k,
            stage2_k,
            at_ns,
        } => Demand::TemperatureHold {
            setpoint: kelvin(setpoint_k)?,
            stage1: kelvin(stage1_k)?,
            stage2: kelvin(stage2_k)?,
            at: Seconds::new(Duration::from_nanos(at_ns).as_secs_f64())
                .expect("finite u64 timestamp"),
        },
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeadSetup {
    /// Stage1, stage2, fixture and jacket temperatures in this order.
    pub initial_k: [f64; 4],
    pub capacity_j_per_k: [f64; 3],
    pub links_w_per_k: [f64; 2],
    pub removal_w: [f64; 2],
}

impl HeadSetup {
    pub fn for_mode(mode: &ModeSetup) -> Self {
        if matches!(mode, ModeSetup::TemperatureHold { .. }) {
            Self {
                initial_k: [20.0, 15.0, 15.75, JACKET_K],
                capacity_j_per_k: [10.0, 2.0, 2.0],
                links_w_per_k: [0.0, 1.0],
                removal_w: [0.0, 0.75],
            }
        } else {
            let [stage1, stage2, tf] = HEAD_K;
            Self {
                initial_k: [stage1, stage2, tf, JACKET_K],
                capacity_j_per_k: CAPACITY_J_PER_K,
                links_w_per_k: HEAD_LINKS_W_PER_K,
                removal_w: DEFAULT_REMOVAL_W,
            }
        }
    }

    pub fn build(&self) -> Result<SyntheticHead, String> {
        let [stage1, stage2, tf, _] = self.initial_k.map(Kelvin::new);
        let [stage1_capacity, stage2_capacity, fixture_capacity] =
            self.capacity_j_per_k.map(JoulesPerKelvin::new);
        let [stage_link, fixture_link] = self.links_w_per_k.map(WattsPerKelvin::new);
        let error = "Параметры синтетической головки должны быть конечными";
        SyntheticHead::new(
            HeadParameters {
                stage1_capacity: stage1_capacity.ok_or(error)?,
                stage2_capacity: stage2_capacity.ok_or(error)?,
                fixture_capacity: fixture_capacity.ok_or(error)?,
                stage_link: stage_link.ok_or(error)?,
                fixture_link: fixture_link.ok_or(error)?,
            },
            HeadTemperatures {
                stage1: stage1.ok_or(error)?,
                stage2: stage2.ok_or(error)?,
                tf: tf.ok_or(error)?,
            },
        )
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HoldSetup {
    pub kp_w_per_k: f64,
    pub ki_w_per_k_s: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModeSnapshot {
    pub mode: ModeSetup,
    pub head: HeadSetup,
    pub hold: Option<HoldSetup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BodySetup>,
}

impl ModeSnapshot {
    pub fn new(mode: ModeSetup) -> Result<Self, String> {
        Self::build(mode, None)
    }

    pub fn with_body(mode: ModeSetup, body: BodySetup) -> Result<Self, String> {
        Self::build(mode, Some(body))
    }

    fn build(mode: ModeSetup, body: Option<BodySetup>) -> Result<Self, String> {
        let head = HeadSetup::for_mode(&mode);
        let hold = matches!(mode, ModeSetup::TemperatureHold { .. }).then_some(HOLD_GAINS);
        let snapshot = Self {
            mode,
            head,
            hold,
            body,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.mode.validate()?;
        let expected = HeadSetup::for_mode(&self.mode);
        let expected_hold =
            matches!(self.mode, ModeSetup::TemperatureHold { .. }).then_some(HOLD_GAINS);
        let hold_bits = |hold: HoldSetup| [hold.kp_w_per_k.to_bits(), hold.ki_w_per_k_s.to_bits()];
        if self.head.initial_k.map(f64::to_bits) != expected.initial_k.map(f64::to_bits)
            || self.head.capacity_j_per_k.map(f64::to_bits)
                != expected.capacity_j_per_k.map(f64::to_bits)
            || self.head.links_w_per_k.map(f64::to_bits) != expected.links_w_per_k.map(f64::to_bits)
            || self.head.removal_w.map(f64::to_bits) != expected.removal_w.map(f64::to_bits)
            || self.hold.map(hold_bits) != expected_hold.map(hold_bits)
            || self.body.is_some() != matches!(self.mode, ModeSetup::VirtualBody { .. })
        {
            return Err("Снимок режима не соответствует синтетическим параметрам test_only".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BodyEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass_kg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<BridgeSetup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g_w_per_k: Option<f64>,
}

#[derive(Clone, Copy)]
pub struct HeldPort {
    temperature: Kelvin,
    read_started_ns: u64,
}

impl HeldPort {
    pub fn temperature_k(self) -> f64 {
        self.temperature.get()
    }

    pub fn read_started_ns(self) -> u64 {
        self.read_started_ns
    }
}

pub struct Compensation {
    fixture: Vec<InventoryPart>,
    parasitic: Watts,
    previous: Option<HeldPort>,
    latest: Option<HeldPort>,
}

impl Compensation {
    pub fn new(snapshot: &ModeSnapshot, catalog: &str) -> Result<Option<Self>, String> {
        snapshot.validate()?;
        let ModeSetup::VirtualBody {
            compensation: Some(setup),
            ..
        } = &snapshot.mode
        else {
            return Ok(None);
        };
        let mut head = snapshot.head.build()?;
        let tf = head.temperatures().tf;
        let fixture = setup
            .fixture
            .iter()
            .map(|part| {
                let cp =
                    nist_heat_capacity_from_catalog(&part.material, catalog).map_err(|_| {
                        format!("Для материала насадки {} нет теплоёмкости", part.material)
                    })?;
                Ok(InventoryPart {
                    mass: Kilograms::new(part.mass_kg).ok_or("Неконечная масса насадки")?,
                    temperature: tf,
                    heat_capacity: cp.curve,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut compensation = Self {
            fixture,
            parasitic: Watts::new(setup.fixture_parasitic_w)
                .ok_or("Неконечный приток к насадке")?,
            previous: None,
            latest: None,
        };
        let zero = Watts::new(0.0).unwrap();
        compensation.advance_head(
            &mut head,
            Seconds::new(PERIOD.as_secs_f64()).unwrap(),
            [zero; 2],
            snapshot
                .head
                .removal_w
                .map(|value| Watts::new(value).unwrap()),
        )?;
        Ok(Some(compensation))
    }

    pub fn sample(
        temperatures: &[TemperatureSample; 4],
        measured_ns: u64,
        max_age: Duration,
    ) -> Option<HeldPort> {
        BodyPort::Tf.sample(temperatures, measured_ns, max_age)
    }

    pub fn evaluate(
        &mut self,
        current: Option<HeldPort>,
        measured_ns: u64,
        max_age: Duration,
    ) -> ModeCompensationV1 {
        let unavailable = |reason| ModeCompensationV1::Unavailable { reason };
        let Some(current) = current
            .filter(|sample| held_age(sample.read_started_ns, measured_ns, max_age).is_some())
        else {
            return unavailable(ModeCompensationIssue::MissingTf);
        };
        let Some(previous) = self
            .latest
            .filter(|sample| held_age(sample.read_started_ns, measured_ns, max_age).is_some())
        else {
            return unavailable(ModeCompensationIssue::PairNotReady);
        };
        let Some(dt) = current
            .read_started_ns
            .checked_sub(previous.read_started_ns)
            .filter(|dt| *dt > 0)
        else {
            return unavailable(ModeCompensationIssue::PairNotReady);
        };
        let rate = (current.temperature_k() - previous.temperature_k())
            / Duration::from_nanos(dt).as_secs_f64();
        let Ok(capacity) = self.capacity_at(current.temperature) else {
            return unavailable(ModeCompensationIssue::FixtureInvalid);
        };
        if !rate.is_finite() {
            return unavailable(ModeCompensationIssue::FixtureInvalid);
        }
        ModeCompensationV1::Ready {
            previous_tf_k: previous.temperature_k(),
            previous_read_started_ns: previous.read_started_ns(),
            current_tf_k: current.temperature_k(),
            current_read_started_ns: current.read_started_ns(),
            d_tf_k_per_s: rate,
            fixture_capacity_j_per_k: capacity.get(),
            fixture_parasitic_w: self.parasitic.get(),
        }
    }

    /// Call only for a journaled observation that remains eligible after M5.
    pub fn accept(&mut self, current: Option<HeldPort>) {
        if let Some(current) = current.filter(|sample| self.capacity_at(sample.temperature).is_ok())
        {
            self.previous = self
                .latest
                .filter(|sample| sample.read_started_ns < current.read_started_ns);
            self.latest = Some(current);
        } else {
            self.clear();
        }
    }

    pub fn clear(&mut self) {
        self.previous = None;
        self.latest = None;
    }

    pub fn ready(&self, now_ns: u64, max_age: Duration) -> bool {
        self.previous
            .zip(self.latest)
            .is_some_and(|(previous, latest)| {
                held_age(previous.read_started_ns, now_ns, max_age).is_some()
                    && held_age(latest.read_started_ns, now_ns, max_age).is_some()
            })
    }

    pub fn advance_head(
        &mut self,
        head: &mut SyntheticHead,
        dt: Seconds,
        heater: [Watts; 2],
        cooling: [Watts; 2],
    ) -> Result<(), String> {
        let capacity = self.capacity_at(head.temperatures().tf)?;
        head.advance_with_fixture(dt, heater, cooling, capacity, self.parasitic)
            .map_err(|error| error.to_string())
    }

    fn capacity_at(&mut self, temperature: Kelvin) -> Result<JoulesPerKelvin, String> {
        for part in &mut self.fixture {
            part.temperature = temperature;
        }
        let capacity = fixture_heat_capacity(&self.fixture)?;
        if capacity.get() <= 0.0 {
            return Err("Теплоёмкость насадки должна быть положительной".into());
        }
        Ok(capacity)
    }
}

pub fn compensation_emulation(evidence: Option<ModeCompensationV1>) -> Option<EmulationMode> {
    match evidence {
        None => Some(EmulationMode::AdditionalLoad),
        Some(ModeCompensationV1::Unavailable { .. }) => None,
        Some(ModeCompensationV1::Ready {
            fixture_capacity_j_per_k,
            d_tf_k_per_s,
            fixture_parasitic_w,
            ..
        }) => Some(EmulationMode::FixtureCompensation {
            capacity: JoulesPerKelvin::new(fixture_capacity_j_per_k)?,
            temperature_rate: KelvinPerSecond::new(d_tf_k_per_s)?,
            parasitic: Watts::new(fixture_parasitic_w)?,
        }),
    }
}

/// Forecast ideal capped heat in the additional-load mode. Compensation starts
/// with a single OFF point because its temperature rate comes from observations.
pub fn preview_body(snapshot: &ModeSnapshot, cap: Watts) -> Result<Vec<A1PreviewPoint>, String> {
    if cap.get() < 0.0 {
        return Err("Недопустимый предел нагрева для прогноза".into());
    }
    let mut state = VirtualBody::new(snapshot, CATALOG.into())?;
    let mut head = snapshot.head.build()?;
    let compensation = Compensation::new(snapshot, CATALOG)?.is_some();
    let cooling = snapshot
        .head
        .removal_w
        .map(|power| Watts::new(power).unwrap());
    let jacket = snapshot.head.initial_k[3];
    let dt = Seconds::new(PERIOD.as_secs_f64()).unwrap();
    let zero = Watts::new(0.0).unwrap();
    let mut points = Vec::with_capacity(PREVIEW_STEPS as usize + 1);
    let point =
        |index, head: HeadTemperatures, body: &Body, flow: Watts, heater: Watts| A1PreviewPoint {
            time_s: (PERIOD * index).as_secs_f64(),
            body_k: body.temperature().get(),
            temperatures_k: [head.stage1.get(), head.stage2.get(), head.tf.get(), jacket],
            bridge_w: flow.get(),
            heater_w: heater.get(),
        };
    for index in 0..PREVIEW_STEPS {
        let temperatures = head.temperatures();
        let port = state.port.temperature(temperatures);
        let flow = state.bridge.flow(state.body.temperature(), port)?;
        let heater = if compensation {
            zero
        } else {
            let target = request(EmulationMode::AdditionalLoad, flow)?.nonnegative_target;
            Watts::new(target.get().min(cap.get())).unwrap()
        };
        if index == 0 {
            points.push(point(0, temperatures, &state.body, flow, heater));
        }
        if compensation {
            break;
        }
        head.advance(dt, [heater, zero], cooling)
            .map_err(|error| error.to_string())?;
        state.body.step(
            dt,
            PortSample {
                temperature: port,
                age: Seconds::new(0.0).unwrap(),
            },
            &state.bridge,
            state.body_heat,
        )?;
        points.push(point(
            index + 1,
            head.temperatures(),
            &state.body,
            flow,
            heater,
        ));
    }
    Ok(points)
}

pub struct VirtualBody {
    setup: BodySetup,
    catalog: String,
    body: Body,
    bridge: Bridge,
    port: BodyPort,
    body_heat: Watts,
    held: Option<HeldPort>,
}

struct PreparedEdit {
    setup: BodySetup,
    body: Option<Body>,
    bridge: Bridge,
    flow: Watts,
    dh_event_j: Option<f64>,
}

impl VirtualBody {
    pub fn new(snapshot: &ModeSnapshot, catalog: String) -> Result<Self, String> {
        snapshot.validate()?;
        let ModeSetup::VirtualBody {
            port, body_heat_w, ..
        } = snapshot.mode
        else {
            return Err("Требуется режим виртуального тела".into());
        };
        let setup = snapshot
            .body
            .clone()
            .ok_or("Отсутствуют параметры виртуального тела")?;
        let (_, channel) = port.channel();
        let initial_port = Kelvin::new(snapshot.head.initial_k[usize::from(channel - 1)])
            .ok_or("Неконечная температура порта")?;
        let (body, bridge) = setup.build(&catalog, initial_port)?;
        Ok(Self {
            setup,
            catalog,
            body,
            bridge,
            port,
            body_heat: Watts::new(body_heat_w).ok_or("Неконечный приток к телу")?,
            held: None,
        })
    }

    pub fn enthalpy(&self) -> Joules {
        self.body.enthalpy()
    }

    pub fn sample(
        &self,
        temperatures: &[TemperatureSample; 4],
        measured_ns: u64,
        max_age: Duration,
    ) -> Option<HeldPort> {
        self.port.sample(temperatures, measured_ns, max_age)
    }

    pub fn observe(&mut self, sample: HeldPort) {
        self.held = Some(sample);
    }

    pub fn advance(
        &mut self,
        interval_ns: u64,
        end_ns: u64,
        max_age: Duration,
    ) -> Result<ModeBodyStepV1, String> {
        end_ns
            .checked_sub(interval_ns)
            .ok_or("Неверные границы теплового шага тела")?;
        let held = self.held.ok_or("Отсутствует удержанное измерение порта")?;
        let age = held_age(held.read_started_ns, end_ns, max_age)
            .ok_or("Удержанное измерение порта просрочено или нарушено время")?;
        let seconds =
            |duration: Duration| Seconds::new(duration.as_secs_f64()).expect("finite duration");
        self.body.step(
            seconds(Duration::from_nanos(interval_ns)),
            PortSample {
                temperature: held.temperature,
                age: seconds(age),
            },
            &self.bridge,
            self.body_heat,
        )?;
        Ok(ModeBodyStepV1 {
            port: self.port.channel().0,
            held_k: held.temperature_k(),
            held_read_started_ns: held.read_started_ns(),
            held_age_ns: end_ns - held.read_started_ns(),
            temperature_k: self.body.temperature().get(),
            q_v_w: None,
            change: None,
            compensation: None,
        })
    }

    pub fn validate_edit(&self, edit: &BodyEdit) -> Result<(), String> {
        let port = self
            .held
            .ok_or("Для изменения тела нужно принятое измерение порта")?;
        self.prepare_edit(edit, port).map(|_| ())
    }

    /// Complete the demand and any edit after the interval, without accepting a new held sample.
    pub fn complete_tick(
        &mut self,
        step: &mut ModeBodyStepV1,
        current: Option<HeldPort>,
        edit: Option<BodyEdit>,
        emulation: Option<EmulationMode>,
    ) -> Result<(f64, Option<f64>), String> {
        if let Some(edit) = edit {
            let raw_request = serde_json::to_value(&edit).map_err(|error| error.to_string())?;
            let candidate = current.zip(emulation).and_then(|(port, mode)| {
                let prepared = self.prepare_edit(&edit, port).ok()?;
                let requested = request(mode, prepared.flow).ok()?.requested.get();
                Some((prepared, requested))
            });
            if let Some((prepared, requested)) = candidate {
                let temperature_before_k = self.body.temperature().get();
                let mut dh_scale = 1.0;
                if let Some(body) = prepared.body {
                    dh_scale = self.enthalpy().get().abs().max(body.enthalpy().get().abs());
                    self.body = body;
                }
                self.setup = prepared.setup;
                self.bridge = prepared.bridge;
                step.temperature_k = self.body.temperature().get();
                step.q_v_w = Some(prepared.flow.get());
                step.change = Some(ModeBodyChangeV1 {
                    request: raw_request,
                    outcome: ModeChangeOutcomeV1::Applied {
                        temperature_before_k,
                        temperature_after_k: step.temperature_k,
                        dh_event_j: prepared.dh_event_j,
                    },
                });
                return Ok((dh_scale, Some(requested)));
            }
            step.change = Some(ModeBodyChangeV1 {
                request: raw_request,
                outcome: ModeChangeOutcomeV1::Rejected,
            });
        }
        let flow = current
            .map(|port| self.bridge.flow(self.body.temperature(), port.temperature))
            .transpose()?;
        step.q_v_w = flow.map(Watts::get);
        let requested = flow
            .zip(emulation)
            .map(|(flow, mode)| request(mode, flow).map(|power| power.requested.get()))
            .transpose()?;
        Ok((1.0, requested))
    }

    fn prepare_edit(&self, edit: &BodyEdit, port: HeldPort) -> Result<PreparedEdit, String> {
        let modifies_body =
            edit.material.is_some() || edit.mass_kg.is_some() || edit.scale.is_some();
        let bridge_setup = match (&edit.bridge, edit.g_w_per_k) {
            (Some(_), Some(_)) => return Err("Нельзя задать одновременно мост и G".into()),
            (Some(bridge), None) => bridge.clone(),
            (None, Some(conductance_w_per_k)) => BridgeSetup::Constant {
                conductance_w_per_k,
            },
            (None, None) if modifies_body => self.setup.bridge.clone(),
            (None, None) => return Err("Не заданы изменения виртуального тела".into()),
        };
        let setup = BodySetup {
            material: edit
                .material
                .as_ref()
                .unwrap_or(&self.setup.material)
                .clone(),
            mass_kg: edit.mass_kg.unwrap_or(self.setup.mass_kg),
            scale: edit.scale.unwrap_or(self.setup.scale),
            temperature_k: self.body.temperature().get(),
            bridge: bridge_setup,
        };
        let (body, bridge) = if modifies_body {
            let (body, bridge) = setup.build(&self.catalog, port.temperature)?;
            (Some(body), bridge)
        } else {
            (None, setup.bridge.build(&self.catalog)?)
        };
        let flow = bridge.flow(
            body.as_ref().unwrap_or(&self.body).temperature(),
            port.temperature,
        )?;
        let dh_event_j = body
            .as_ref()
            .map(|body| body.enthalpy().get() - self.enthalpy().get());
        if dh_event_j.is_some_and(|event| !event.is_finite()) {
            return Err("Неконечное изменение энтальпии".into());
        }
        Ok(PreparedEdit {
            setup,
            body,
            bridge,
            flow,
            dh_event_j,
        })
    }
}
