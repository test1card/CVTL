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
//   ThermalInput - raw tf observation and optional step
//   Parameters - recorded initial body parameters
//   Summary - verified thermal prefix
// END_MODULE_MAP

use std::time::Duration;

use cvtl_control::Demand;
use cvtl_core::units::{Kelvin, Kilograms, Seconds, Watts, WattsPerKelvin};
use cvtl_materials::{nist_heat_capacity_from_catalog, HeatCapacityCurve};
use cvtl_physics::{request, Body, Bridge, EmulationMode, PortSample};
use cvtl_store::record::{A1StepV1, Quality, TemperatureSample};
use serde::{Deserialize, Serialize};

use super::{Diagnostic, Frame, Operation};

#[derive(Deserialize)]
pub(super) struct Parameters {
    synthetic: bool,
    material: String,
    body_mass_kg: f64,
    body_temperature_k: f64,
    scale: f64,
    bridge_w_per_k: f64,
    port: String,
    mode: String,
    body_parasitic_w: f64,
}

#[derive(Clone, Copy)]
pub(super) struct ThermalInput {
    pub tf: TemperatureSample,
    pub step: Option<A1StepV1>,
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
    cp: HeatCapacityCurve,
    body: Body,
    mass: Kilograms,
    bridge: Bridge,
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
        if !p.synthetic
            || p.port != "tf"
            || p.mode != "additional_load"
            || p.body_parasitic_w != 0.0
        {
            return Err("Неподдерживаемые параметры теплового профиля A1".into());
        }
        let mass = Kilograms::new(p.body_mass_kg).ok_or("Неверная масса тела A1")?;
        let temperature =
            Kelvin::new(p.body_temperature_k).ok_or("Неверная начальная температура тела A1")?;
        let cp = nist_heat_capacity_from_catalog(&p.material, catalog)
            .map_err(|error| format!("Непригодный каталог nist_csv для A1 ({error:?})"))?
            .curve;
        let g = WattsPerKelvin::new(p.bridge_w_per_k)
            .filter(|g| g.get() >= 0.0)
            .ok_or("Неверный тепловой мост A1")?;
        let body = Body::new(cp.clone(), mass, p.scale, temperature)
            .map_err(|_| "Непригодные начальные параметры тела A1")?;
        Ok(Self {
            cp,
            body,
            mass,
            bridge: Bridge::Constant(g),
            max_age: Duration::from_millis(max_age_ms),
            held: None,
            start_ns: 0,
            summary: Summary::default(),
        })
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
        if !frame.monitor.model_valid || !same(step.held_tf_k, held.temperature.get()) {
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
            let before = self.body.temperature().get();
            if !same(before, change.temperature_before_k) {
                return Err(op.mismatch(frame.seq, "Температура до смены параметров A1 отличается"));
            }
            if let Some(scale) = change.scale {
                let dh = self
                    .body
                    .reconfigure(self.cp.clone(), self.mass, scale)
                    .map_err(|_| op.mismatch(frame.seq, "Недопустимое изменение scale A1"))?;
                if !change
                    .dh_event_j
                    .is_some_and(|recorded| same(dh.get(), recorded))
                {
                    return Err(op.mismatch(frame.seq, "Пересчитанное dH_event A1 отличается"));
                }
            }
            if let Some(bridge) = change.bridge_w_per_k {
                self.bridge = Bridge::Constant(WattsPerKelvin::new(bridge).unwrap());
            }
            if !same(self.body.temperature().get(), change.temperature_after_k) {
                return Err(op.mismatch(
                    frame.seq,
                    "Температура после смены параметров A1 отличается",
                ));
            }
        }
        if !same(self.body.temperature().get(), step.body_temperature_k) {
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
            if !matches!(frame.tick.demand, Demand::VirtualBody(recorded) if same(recorded.get(), requested.get()))
            {
                return Err(op.mismatch(frame.seq, "Пересчитанный запрос тела A1 отличается"));
            }
            self.held = Some(current);
            Demand::VirtualBody(requested)
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
        let tf = frame.thermal?.tf;
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

fn same(left: f64, right: f64) -> bool {
    left.to_bits() == right.to_bits()
}
