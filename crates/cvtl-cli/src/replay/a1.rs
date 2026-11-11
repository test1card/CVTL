// FILE: crates/cvtl-cli/src/replay/a1.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Recompute the virtual body from recorded A1 observations
//   SCOPE: Archived thermal parameters, held thermometry and causal step comparison
//   DEPENDS: M-CVTL-CORE, M-CVTL-MATERIALS, M-CVTL-PHYSICS, M-CVTL-CONTROL, M-CVTL-STORE
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   ThermalReplay - one state of the replayed virtual body and bridge
//   ThermalInput - raw A1 thermometry and optional step
//   Parameters - recorded initial body parameters
//   Summary - verified thermal prefix
// END_MODULE_MAP

use std::time::Duration;

use cvtl_control::Demand;
use cvtl_core::units::{Kelvin, Kilograms, Seconds, Watts, WattsPerKelvin};
use cvtl_materials::nist_heat_capacity_from_catalog;
use cvtl_physics::{request, scenario::BodySetup, Body, Bridge, EmulationMode, PortSample};
use cvtl_store::record::{A1StepV1, Quality, TemperatureSample};
use serde::{Deserialize, Serialize};

use super::{Diagnostic, Frame, Operation};

#[derive(Deserialize)]
struct ModeParameters {
    synthetic: bool,
    port: String,
    mode: String,
    body_parasitic_w: f64,
}

impl ModeParameters {
    fn validate(&self) -> Result<(), String> {
        if self.synthetic
            && self.port == "tf"
            && self.mode == "additional_load"
            && self.body_parasitic_w == 0.0
        {
            Ok(())
        } else {
            Err("Неподдерживаемые параметры теплового профиля A1".into())
        }
    }
}

#[derive(Deserialize)]
pub(super) struct Parameters {
    #[serde(flatten)]
    mode: ModeParameters,
    material: String,
    body_mass_kg: f64,
    body_temperature_k: f64,
    scale: f64,
    bridge_w_per_k: f64,
}

#[derive(Deserialize)]
pub(super) struct ConfiguredParameters {
    #[serde(flatten)]
    mode: ModeParameters,
    head_temperature_k: [f64; 3],
    setup: BodySetup,
}

#[derive(Clone, Copy)]
pub(super) struct ThermalInput {
    pub temperatures: [TemperatureSample; 4],
    pub step: Option<A1StepV1>,
}

impl ThermalInput {
    pub(super) fn verified_temperatures(&self, end_ns: u64, measured_ns: u64) -> Option<[f64; 4]> {
        let read = |index: usize| {
            let sample = self.temperatures[index];
            let started = sample.read_started_ns?;
            let finished = sample.read_finished_ns?;
            let temperature = sample.temperature_k?;
            (sample.quality == Quality::Measured
                && sample.channel == Some(index as u8 + 1)
                && temperature.is_finite()
                && temperature > 0.0
                && end_ns <= started
                && started <= finished
                && finished <= measured_ns)
                .then_some(temperature)
        };
        Some([read(0)?, read(1)?, read(2)?, read(3)?])
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct Summary {
    thermal_steps: u64,
    last_interval_end_ns: Option<u64>,
}

#[derive(Clone, Copy)]
struct Held {
    temperature: Kelvin,
    started_ns: u64,
}

pub(super) struct ThermalReplay {
    body: Body,
    bridge: Bridge,
    allow_mass: bool,
    max_age: Duration,
    held: Option<Held>,
    start_ns: u64,
    pub summary: Summary,
}

impl ThermalReplay {
    pub(super) fn new(
        parameters: &Parameters,
        catalog: &str,
        max_age_ms: u64,
    ) -> Result<Self, String> {
        let p = parameters;
        p.mode.validate()?;
        let mass = Kilograms::new(p.body_mass_kg).ok_or("Неверная масса тела A1")?;
        let temperature =
            Kelvin::new(p.body_temperature_k).ok_or("Неверная начальная температура тела A1")?;
        let cp = nist_heat_capacity_from_catalog(&p.material, catalog)
            .map_err(|error| format!("Непригодный каталог nist_csv для A1 ({error:?})"))?
            .curve;
        let g = WattsPerKelvin::new(p.bridge_w_per_k)
            .filter(|g| g.get() >= 0.0)
            .ok_or("Неверный тепловой мост A1")?;
        let body = Body::new(cp, mass, p.scale, temperature)
            .map_err(|_| "Непригодные начальные параметры тела A1")?;
        Ok(Self::with_body(
            body,
            Bridge::Constant(g),
            max_age_ms,
            false,
        ))
    }

    pub(super) fn configured(
        parameters: &ConfiguredParameters,
        catalog: &str,
        max_age_ms: u64,
    ) -> Result<Self, String> {
        parameters.mode.validate()?;
        let tf = Kelvin::new(parameters.head_temperature_k[2]).ok_or("Неверное исходное tf A1")?;
        let (body, bridge) = parameters.setup.build(catalog, tf).map_err(|error| {
            format!("Непригодные начальные параметры тела или моста A1 ({error})")
        })?;
        Ok(Self::with_body(body, bridge, max_age_ms, true))
    }

    fn with_body(body: Body, bridge: Bridge, max_age_ms: u64, allow_mass: bool) -> Self {
        Self {
            body,
            bridge,
            allow_mass,
            max_age: Duration::from_millis(max_age_ms),
            held: None,
            start_ns: 0,
            summary: Summary::default(),
        }
    }

    pub(super) fn start(&mut self, frame: &Frame, op: &Operation) -> Result<(), Diagnostic> {
        if self.held.is_none() {
            self.held = Some(
                self.observe_tf(frame)
                    .ok_or_else(|| op.insufficient("Нет пригодного tf первого Start A1"))?,
            );
            self.start_ns = frame.measured_ns();
        }
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        frame: &Frame,
        op: &Operation,
    ) -> Result<(Demand, Option<u64>), Diagnostic> {
        let input = frame.thermal.expect("A1 profile has thermal inputs");
        let Some(step) = input.step else {
            if self.held.is_some() && frame.monitor.model_valid {
                return Err(op.insufficient("Не записан обязательный тепловой шаг A1"));
            }
            if !matches!(frame.tick.demand, Demand::Observe) {
                return Err(op.mismatch(frame.seq, "Запрос A1 без записанного теплового шага"));
            }
            return Ok((Demand::Observe, None));
        };
        let held = self
            .held
            .ok_or_else(|| op.mismatch(frame.seq, "Тепловой шаг A1 до Start"))?;
        if !frame.monitor.model_valid
            || step.held_tf_k.to_bits() != held.temperature.get().to_bits()
        {
            return Err(op.mismatch(
                frame.seq,
                "Удержанное наблюдение tf отличается от входов A1",
            ));
        }
        let age = Duration::from_nanos(step.held_tf_age_ns);
        let end = held
            .started_ns
            .checked_add(step.held_tf_age_ns)
            .filter(|end| *end <= frame.measured_ns() && age <= self.max_age)
            .ok_or_else(|| op.mismatch(frame.seq, "Недопустимое время или возраст tf A1"))?;
        let begin = end
            .checked_sub(step.interval_ns)
            .filter(|begin| {
                *begin >= self.start_ns
                    && self
                        .summary
                        .last_interval_end_ns
                        .is_none_or(|prior| prior == *begin)
            })
            .ok_or_else(|| {
                op.mismatch(frame.seq, "Нарушена непрерывность тепловых интервалов A1")
            })?;
        let dt = Seconds::new(Duration::from_nanos(end - begin).as_secs_f64()).unwrap();
        let sample = PortSample {
            temperature: held.temperature,
            age: Seconds::new(age.as_secs_f64()).unwrap(),
        };
        self.body
            .step(dt, sample, &self.bridge, Watts::new(0.0).unwrap())
            .map_err(|_| op.mismatch(frame.seq, "Не удалось пересчитать тепловой шаг A1"))?;
        if let Some(change) = step.change {
            if change.mass_kg.is_some() && !self.allow_mass {
                return Err(op.mismatch(
                    frame.seq,
                    "Изменение массы не поддерживается в профиле A1 v1",
                ));
            }
            if change.bridge_w_per_k.is_some() && !matches!(self.bridge, Bridge::Constant(_)) {
                return Err(op.mismatch(frame.seq, "Изменение G доступно для постоянного моста"));
            }
            let before = self.body.temperature().get();
            if !within_thermal_roundoff(before, change.temperature_before_k, 1.0) {
                return Err(op.mismatch(frame.seq, "Температура до смены параметров A1 отличается"));
            }
            if change.mass_kg.is_some() || change.scale.is_some() {
                let mass = change.mass_kg.map(|mass| Kilograms::new(mass).unwrap());
                let enthalpy_before = self.body.enthalpy().get();
                let dh = self
                    .body
                    .reconfigure_mass_scale(mass, change.scale)
                    .map_err(|_| {
                        op.mismatch(frame.seq, "Недопустимое изменение массы или scale A1")
                    })?;
                let enthalpy_scale = enthalpy_before.abs().max(self.body.enthalpy().get().abs());
                if !change.dh_event_j.is_some_and(|recorded| {
                    within_thermal_roundoff(dh.get(), recorded, enthalpy_scale)
                }) {
                    return Err(op.mismatch(frame.seq, "Пересчитанное dH_event A1 отличается"));
                }
            }
            if let Some(bridge) = change.bridge_w_per_k {
                self.bridge = Bridge::Constant(WattsPerKelvin::new(bridge).unwrap());
            }
            if !within_thermal_roundoff(
                self.body.temperature().get(),
                change.temperature_after_k,
                1.0,
            ) {
                return Err(op.mismatch(
                    frame.seq,
                    "Температура после смены параметров A1 отличается",
                ));
            }
        }
        if !within_thermal_roundoff(self.body.temperature().get(), step.body_temperature_k, 1.0) {
            return Err(op.mismatch(frame.seq, "Пересчитанная температура тела A1 отличается"));
        }
        let demand = if let Some(current) = self.observe_tf(frame) {
            if current.started_ns < end {
                return Err(op.mismatch(frame.seq, "Текущее tf предшествует концу шага A1"));
            }
            let requested = self
                .bridge
                .flow(self.body.temperature(), current.temperature)
                .and_then(|flow| request(EmulationMode::AdditionalLoad, flow))
                .map_err(|_| op.mismatch(frame.seq, "Недопустимый запрос тела A1"))?
                .requested;
            if !matches!(frame.tick.demand, Demand::VirtualBody(recorded)
                if within_thermal_roundoff(requested.get(), recorded.get(), 1.0))
            {
                return Err(op.mismatch(frame.seq, "Пересчитанный запрос тела A1 отличается"));
            }
            self.held = Some(current);
            // M4/M5 must replay the verified input used by the original tick.
            frame.tick.demand
        } else if !frame.monitor.thermometer_connected
            && matches!(frame.tick.demand, Demand::Observe)
        {
            Demand::Observe
        } else {
            return Err(op.mismatch(frame.seq, "Нет связанного текущего наблюдения tf A1"));
        };
        Ok((demand, Some(end)))
    }

    pub(super) fn verify_boundary(&mut self, end_ns: u64) {
        self.summary.thermal_steps += 1;
        self.summary.last_interval_end_ns = Some(end_ns);
    }

    fn observe_tf(&self, frame: &Frame) -> Option<Held> {
        let tf = frame.thermal?.temperatures[2];
        let started_ns = tf.read_started_ns?;
        let age = Duration::from_nanos(frame.measured_ns().checked_sub(started_ns)?);
        let temperature = Kelvin::new(tf.temperature_k?)?;
        (tf.quality == Quality::Measured
            && tf.channel == Some(3)
            && temperature.get() > 0.0
            && age <= self.max_age)
            .then_some(Held {
                temperature,
                started_ns,
            })
    }
}

fn within_thermal_roundoff(computed: f64, recorded: f64, scale: f64) -> bool {
    let magnitude = computed.abs().max(recorded.abs()).max(scale).max(1.0);
    computed.is_finite()
        && recorded.is_finite()
        && (computed - recorded).abs() <= (64.0 * f64::EPSILON) * magnitude
}

#[cfg(test)]
mod tests {
    use super::within_thermal_roundoff;

    #[test]
    fn thermal_roundoff_has_an_inclusive_scaled_boundary() {
        let limit = 64.0 * f64::EPSILON;
        assert!(within_thermal_roundoff(0.0, -0.0, 1.0));
        assert!(within_thermal_roundoff(0.0, limit, 1.0));
        assert!(!within_thermal_roundoff(0.0, limit.next_up(), 1.0));

        let inside = 32.0 - 32.0 * limit;
        assert!(within_thermal_roundoff(32.0, inside, 1.0));
        assert!(!within_thermal_roundoff(32.0, inside.next_down(), 1.0));
        assert!(within_thermal_roundoff(
            -128.0 * limit,
            128.0 * limit,
            256.0
        ));
        assert!(!within_thermal_roundoff(
            -256.0 * limit,
            256.0 * limit,
            256.0
        ));
    }

    #[test]
    fn thermal_roundoff_rejects_nonfinite_values_and_overflow() {
        assert!(!within_thermal_roundoff(f64::MAX, -f64::MAX, 1.0));
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!within_thermal_roundoff(invalid, 0.0, 1.0));
            assert!(!within_thermal_roundoff(0.0, invalid, 1.0));
            assert!(!within_thermal_roundoff(invalid, invalid, 1.0));
        }
    }
}
