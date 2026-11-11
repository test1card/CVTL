// FILE: crates/cvtl-sim/src/a1.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Step the test-only A1 thermal scenario
//   SCOPE: Actual heater energy, held thermometry and virtual body
//   DEPENDS: M-CVTL-CORE, M-CVTL-MATERIALS, M-CVTL-PHYSICS, M-CVTL-STORE
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   A1Scenario - single private thermal state and scenario snapshot
//   HeldTf - validated Lake Shore observation held between steps
//   A1PreviewPoint - bounded forecast sample
//   A1Calibration - test-only applied heat and recorded boundary inputs
//   A1OtherHeat - measured or concealed applied heat
// END_MODULE_MAP
use std::{fmt::Write, time::Duration};

use cvtl_core::units::{JoulesPerKelvin, Kelvin, Kilograms, Seconds, Watts, WattsPerKelvin};
use cvtl_materials::nist_heat_capacity;
use cvtl_physics::{
    request,
    scenario::{BodySetup, BridgeSetup},
    Body, Bridge, EmulationMode, PortSample, PowerRequest,
};
use cvtl_store::record::{A1ChangeV1, A1StepV1, Inputs, Quality, Snapshot, MAX_RUN_HEADER_BYTES};

use crate::{
    head::{HeadParameters, HeadTemperatures, SyntheticHead},
    keithley::EnergyInterval,
};

pub(super) const SCENARIO: &str = "test_only_a1_loop_v1";
pub(super) const CONFIGURED_SCENARIO: &str = "test_only_a1_loop_v2";
const CATALOG: &str = include_str!("../../../data/materials/nist.csv");
const MATERIAL: &str = "ofhc_rrr100";
const MASS_KG: f64 = 20.0;
const BODY_K: f64 = 30.0;
const INITIAL_SCALE: f64 = 0.1;
const INITIAL_G: f64 = 0.02;
const HEAD_K: [f64; 3] = [45.0, 15.0, 16.0];
const JACKET_K: f64 = 77.0;
const CAPACITY_J_PER_K: [f64; 3] = [10.0, 20.0, 5.0];
const HEAD_LINKS_W_PER_K: [f64; 2] = [0.05, 0.5];
const PREVIEW_STEPS: u32 = 30;
const DEFAULT_REMOVAL_W: [f64; 2] = [2.0, 4.0];

/// An explicitly selected synthetic calibration scenario.
pub struct A1Calibration {
    pub cold_removal: Watts,
    pub other_heat: A1OtherHeat,
    pub wire: Option<BridgeSetup>,
    pub group: Option<String>,
}

#[derive(Clone, Copy)]
pub enum A1OtherHeat {
    Known(Watts),
    Unmeasured(Watts),
}

/// A point in the synthetic forecast. Power values describe the interval ending
/// at this point. At time zero they describe the planned first interval.
#[derive(Clone, Copy, Debug)]
pub struct A1PreviewPoint {
    pub time_s: f64,
    pub body_k: f64,
    /// Stage1, stage2, fixture and jacket temperatures in this order.
    pub temperatures_k: [f64; 4],
    pub bridge_w: f64,
    pub heater_w: f64,
}

#[derive(Clone, Copy)]
pub(super) struct HeldTf {
    temperature: Kelvin,
    read_started_ns: u64,
}

impl HeldTf {
    pub(super) fn from_inputs(input: &Inputs<'_>, max_age: Duration) -> Option<Self> {
        let tf = input.temperatures[2];
        let read_started_ns = tf.read_started_ns?;
        held_age(read_started_ns, input.measured_ns, max_age)?;
        let temperature = Kelvin::new(tf.temperature_k?)?;
        (tf.quality == Quality::Measured && temperature.get() > 0.0).then_some(Self {
            temperature,
            read_started_ns,
        })
    }
}

struct Change {
    mass: Option<Kilograms>,
    scale: Option<f64>,
    bridge: Option<WattsPerKelvin>,
}

pub(super) struct A1Scenario {
    body: Body,
    bridge: Bridge,
    head: SyntheticHead,
    net_cold_removal: Watts,
    applied_wire: Option<Bridge>,
    allow_mass: bool,
    pending: Option<Change>,
    pub(super) held: Option<HeldTf>,
    pub(super) failed: bool,
}

impl A1Scenario {
    pub(super) fn new() -> Self {
        let cp = nist_heat_capacity(MATERIAL).unwrap().curve;
        let body = Body::new(
            cp,
            Kilograms::new(MASS_KG).unwrap(),
            INITIAL_SCALE,
            Kelvin::new(BODY_K).unwrap(),
        )
        .unwrap();
        Self::with_body(
            body,
            Bridge::Constant(WattsPerKelvin::new(INITIAL_G).unwrap()),
            false,
        )
    }

    pub(super) fn configured(setup: &BodySetup) -> Result<Self, String> {
        let (body, bridge) = setup.build(CATALOG, Kelvin::new(HEAD_K[2]).unwrap())?;
        Ok(Self::with_body(body, bridge, true))
    }

    pub(super) fn calibrated(setup: &BodySetup, input: &A1Calibration) -> Result<Self, String> {
        if input.cold_removal.get() < 0.0 {
            return Err("Отвод тепла в калибровочном сценарии должен быть неотрицательным".into());
        }
        let mut scenario = Self::configured(setup)?;
        let (A1OtherHeat::Known(other) | A1OtherHeat::Unmeasured(other)) = input.other_heat;
        scenario.net_cold_removal = Watts::new(input.cold_removal.get() - other.get())
            .ok_or("Холодный отвод с учётом притока вне численного диапазона")?;
        if let Some(wire) = &input.wire {
            if !matches!(wire, BridgeSetup::Conductor { .. }) {
                return Err("Применённый теплоприток проводки требует модели проводника".into());
            }
            scenario.applied_wire = Some(wire.build(CATALOG)?);
        }
        scenario.effective_removals()?;
        Ok(scenario)
    }

    fn with_body(body: Body, bridge: Bridge, allow_mass: bool) -> Self {
        let [stage1, stage2, tf] = HEAD_K.map(|t| Kelvin::new(t).unwrap());
        let [stage1_capacity, stage2_capacity, fixture_capacity] =
            CAPACITY_J_PER_K.map(|c| JoulesPerKelvin::new(c).unwrap());
        let [stage_link, fixture_link] =
            HEAD_LINKS_W_PER_K.map(|g| WattsPerKelvin::new(g).unwrap());
        Self {
            body,
            bridge,
            head: SyntheticHead::new(
                HeadParameters {
                    stage1_capacity,
                    stage2_capacity,
                    fixture_capacity,
                    stage_link,
                    fixture_link,
                },
                HeadTemperatures { stage1, stage2, tf },
            )
            .unwrap(),
            net_cold_removal: Watts::new(DEFAULT_REMOVAL_W[1]).unwrap(),
            applied_wire: None,
            allow_mass,
            pending: None,
            held: None,
            failed: false,
        }
    }

    pub(super) fn snapshot_parameters(
        snapshot: &mut Snapshot,
        setup: Option<&BodySetup>,
        calibration: Option<&A1Calibration>,
    ) -> Result<(), String> {
        snapshot
            .config_yaml
            .push_str("  a1:\n    synthetic: true\n");
        if let Some(setup) = setup {
            let yaml = yaml_serde::to_string(setup)
                .map_err(|error| format!("Не удалось записать конфигурацию тела A1 ({error})"))?;
            snapshot.config_yaml.push_str("    setup:\n");
            for line in yaml.lines() {
                writeln!(snapshot.config_yaml, "      {line}").unwrap();
            }
        } else {
            write!(
                snapshot.config_yaml,
                concat!(
                    "    material: {}\n    body_mass_kg: {}\n    body_temperature_k: {}\n",
                    "    scale: {}\n    bridge_w_per_k: {}\n",
                ),
                MATERIAL, MASS_KG, BODY_K, INITIAL_SCALE, INITIAL_G
            )
            .unwrap();
        }
        write!(snapshot.config_yaml, concat!(
            "    port: tf\n    mode: additional_load\n    body_parasitic_w: 0\n",
            "    head_temperature_k: {:?}\n    head_capacity_j_per_k: {:?}\n    head_links_w_per_k: {:?}\n    jacket_k: {}\n",
        ), HEAD_K, CAPACITY_J_PER_K,
            HEAD_LINKS_W_PER_K, JACKET_K).unwrap();
        if let Some(calibration) = calibration {
            let other_w = match calibration.other_heat {
                A1OtherHeat::Known(value) => Some(value.get()),
                A1OtherHeat::Unmeasured(_) => None,
            };
            let mut inputs = yaml_serde::Mapping::new();
            for (key, value) in [
                ("other_w", yaml_serde::to_value(other_w)),
                ("wire", yaml_serde::to_value(&calibration.wire)),
                ("group", yaml_serde::to_value(&calibration.group)),
            ] {
                inputs.insert(
                    key.into(),
                    value.map_err(|error| format!("Не удалось записать параметры M8 ({error})"))?,
                );
            }
            let yaml = yaml_serde::to_string(&inputs)
                .map_err(|error| format!("Не удалось записать параметры M8 ({error})"))?;
            snapshot.config_yaml.push_str("    m8:\n");
            for line in yaml.lines() {
                writeln!(snapshot.config_yaml, "      {line}").unwrap();
            }
        }
        if snapshot.config_yaml.len() > MAX_RUN_HEADER_BYTES {
            return Err("Снимок настроек A1 превышает допустимый размер".into());
        }
        snapshot
            .references
            .push(("nist_csv".into(), CATALOG.as_bytes().to_vec()));
        Ok(())
    }

    pub(super) fn temperatures(&self) -> [Kelvin; 8] {
        let mut temperatures = [Kelvin::new(42.0).unwrap(); 8];
        temperatures[..4].copy_from_slice(&self.monitored_temperatures());
        temperatures
    }

    fn monitored_temperatures(&self) -> [Kelvin; 4] {
        let head = self.head.temperatures();
        [
            head.stage1,
            head.stage2,
            head.tf,
            Kelvin::new(JACKET_K).unwrap(),
        ]
    }

    pub(super) fn preview(
        mut self,
        cap: Watts,
        period: Duration,
    ) -> Result<Vec<A1PreviewPoint>, String> {
        if cap.get() < 0.0 {
            return Err("Недопустимый предел нагрева для прогноза A1".into());
        }
        let dt = Seconds::new(period.as_secs_f64()).unwrap();
        let mut points = Vec::with_capacity(PREVIEW_STEPS as usize + 1);
        for index in 0..PREVIEW_STEPS {
            let tf = self.head.temperatures().tf;
            let request = self.power_request(tf)?;
            let heater = Watts::new(request.nonnegative_target.get().min(cap.get())).unwrap();
            if index == 0 {
                points.push(self.preview_point(Duration::ZERO, request.requested, heater));
            }
            self.advance_thermal(
                dt,
                [heater, Watts::new(0.0).unwrap()],
                PortSample {
                    temperature: tf,
                    age: Seconds::new(0.0).unwrap(),
                },
            )?;
            points.push(self.preview_point(period * (index + 1), request.requested, heater));
        }
        Ok(points)
    }

    fn preview_point(&self, time: Duration, flow: Watts, heater: Watts) -> A1PreviewPoint {
        A1PreviewPoint {
            time_s: time.as_secs_f64(),
            body_k: self.body.temperature().get(),
            temperatures_k: self.monitored_temperatures().map(Kelvin::get),
            bridge_w: flow.get(),
            heater_w: heater.get(),
        }
    }

    fn advance_thermal(
        &mut self,
        dt: Seconds,
        heater: [Watts; 2],
        port: PortSample,
    ) -> Result<(), String> {
        let removal = self.effective_removals()?;
        self.head
            .advance(dt, heater, removal)
            .map_err(|error| error.to_string())?;
        self.body
            .step(dt, port, &self.bridge, Watts::new(0.0).unwrap())?;
        Ok(())
    }

    fn effective_removals(&self) -> Result<[Watts; 2], String> {
        let t = self.head.temperatures();
        let wire_w = self
            .applied_wire
            .as_ref()
            .map(|wire| wire.flow(t.stage1, t.stage2))
            .transpose()?
            .map_or(0.0, Watts::get);
        let [warm, cold] = [
            DEFAULT_REMOVAL_W[0] + wire_w,
            self.net_cold_removal.get() - wire_w,
        ]
        .map(|value| Watts::new(value).filter(|watts| watts.get() >= 0.0));
        match (warm, cold) {
            (Some(warm), Some(cold)) => Ok([warm, cold]),
            _ => Err("Эффективные отводы тепла должны быть конечными и неотрицательными".into()),
        }
    }

    pub(super) fn queue_change(
        &mut self,
        mass: Option<Kilograms>,
        scale: Option<f64>,
        bridge: Option<WattsPerKelvin>,
    ) -> bool {
        let body_change = mass.is_some() || scale.is_some();
        if self.failed
            || self.pending.is_some()
            || (!body_change && bridge.is_none())
            || (mass.is_some() && !self.allow_mass)
            || (body_change && self.body.validate_mass_scale(mass, scale).is_err())
        {
            return false;
        }
        if let Some(g) = bridge {
            let tf = self
                .held
                .map_or_else(|| self.head.temperatures().tf, |held| held.temperature);
            if !matches!(self.bridge, Bridge::Constant(_))
                || Bridge::Constant(g)
                    .flow(self.body.temperature(), tf)
                    .is_err()
            {
                return false;
            }
        }
        self.pending = Some(Change {
            mass,
            scale,
            bridge,
        });
        true
    }

    pub(super) fn advance(
        &mut self,
        interval: EnergyInterval,
        interval_end_ns: u64,
        max_age: Duration,
    ) -> Option<A1StepV1> {
        let held = self.held?;
        let age = held_age(held.read_started_ns, interval_end_ns, max_age)?;
        let seconds = interval.duration.as_secs_f64();
        let dt = Seconds::new(seconds)?;
        let heater_energy_j = interval.energy.map(|e| e.get());
        let [a, b] = heater_energy_j.map(|e| Watts::new(e / seconds));
        self.advance_thermal(
            dt,
            [a?, b?],
            PortSample {
                temperature: held.temperature,
                age: Seconds::new(age.as_secs_f64())?,
            },
        )
        .ok()?;

        let mut change = None;
        if let Some(next) = self.pending.take() {
            let temperature_before_k = self.body.temperature().get();
            let dh_event_j = if next.mass.is_some() || next.scale.is_some() {
                Some(
                    self.body
                        .reconfigure_mass_scale(next.mass, next.scale)
                        .ok()?
                        .get(),
                )
            } else {
                None
            };
            if let Some(bridge) = next.bridge {
                self.bridge = Bridge::Constant(bridge);
            }
            change = Some(A1ChangeV1 {
                mass_kg: next.mass.map(Kilograms::get),
                scale: next.scale,
                bridge_w_per_k: next.bridge.map(WattsPerKelvin::get),
                dh_event_j,
                temperature_before_k,
                temperature_after_k: self.body.temperature().get(),
            });
        }
        Some(A1StepV1 {
            interval_ns: interval.duration.as_nanos().try_into().ok()?,
            heater_energy_j,
            held_tf_k: held.temperature.get(),
            held_tf_age_ns: age.as_nanos().try_into().ok()?,
            body_temperature_k: self.body.temperature().get(),
            change,
        })
    }

    pub(super) fn demand(&self, current: HeldTf) -> Option<Watts> {
        Some(self.power_request(current.temperature).ok()?.requested)
    }

    fn power_request(&self, tf: Kelvin) -> Result<PowerRequest, String> {
        let flow = self.bridge.flow(self.body.temperature(), tf)?;
        request(EmulationMode::AdditionalLoad, flow)
    }
}

fn held_age(read_started_ns: u64, interval_end_ns: u64, max_age: Duration) -> Option<Duration> {
    let age = Duration::from_nanos(interval_end_ns.checked_sub(read_started_ns)?);
    (age <= max_age).then_some(age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::preview_a1;
    use cvtl_physics::scenario::BridgeSetup;

    #[test]
    fn held_tf_freshness_includes_equality_and_rejects_reversed_time() {
        let max_age = Duration::from_millis(1500);
        assert_eq!(held_age(10, 1_500_000_010, max_age), Some(max_age));
        assert_eq!(held_age(10, 1_500_000_011, max_age), None);
        assert_eq!(held_age(10, 9, max_age), None);
    }

    fn ofhc(temperature_k: f64) -> BodySetup {
        BodySetup {
            material: "ofhc_rrr100".into(),
            mass_kg: 20.0,
            temperature_k,
            scale: 0.1,
            bridge: BridgeSetup::Constant {
                conductance_w_per_k: 0.02,
            },
        }
    }

    #[test]
    fn calibration_transfers_wire_heat_and_applies_hidden_heat_at_stage2() {
        let setup = ofhc(30.0);
        let calibration = A1Calibration {
            cold_removal: Watts::new(5.0).unwrap(),
            other_heat: A1OtherHeat::Unmeasured(Watts::new(1.0).unwrap()),
            wire: Some(BridgeSetup::Conductor {
                material: "ofhc_rrr100".into(),
                area_m2: 1e-7,
                length_m: 0.20,
            }),
            group: Some("calibration-pair".into()),
        };
        let mut scenario = A1Scenario::calibrated(&setup, &calibration).unwrap();
        let wire_w = calibration
            .wire
            .as_ref()
            .unwrap()
            .build(CATALOG)
            .unwrap()
            .flow(Kelvin::new(45.0).unwrap(), Kelvin::new(15.0).unwrap())
            .unwrap()
            .get();
        scenario
            .advance_thermal(
                Seconds::new(0.1).unwrap(),
                [Watts::new(0.0).unwrap(); 2],
                PortSample {
                    temperature: Kelvin::new(16.0).unwrap(),
                    age: Seconds::new(0.0).unwrap(),
                },
            )
            .unwrap();
        let t = scenario.monitored_temperatures().map(Kelvin::get);
        assert!((t[0] - (44.965 - 0.1 * wire_w / 10.0)).abs() < 1e-12);
        assert!((t[1] - (15.0 + 0.1 * (-5.0 + 1.0 + wire_w + 1.5 + 0.5) / 20.0)).abs() < 1e-12);
        assert!((t[2] - 15.99).abs() < 1e-12);
        let head_energy = 10.0 * (t[0] - 45.0) + 20.0 * (t[1] - 15.0) + 5.0 * (t[2] - 16.0);
        assert!((head_energy + 0.1 * (2.0 + 5.0 - 1.0)).abs() < 1e-12);

        let mut snapshot = Snapshot {
            config_yaml: "test_only:\n".into(),
            scenario: CONFIGURED_SCENARIO.into(),
            references: Vec::new(),
        };
        A1Scenario::snapshot_parameters(&mut snapshot, Some(&setup), Some(&calibration)).unwrap();
        let parsed: yaml_serde::Value = yaml_serde::from_str(&snapshot.config_yaml).unwrap();
        let section = parsed["test_only"]["a1"]["m8"].as_mapping().unwrap();
        assert_eq!(section.len(), 3);
        assert!(section.get("other_w").unwrap().is_null());
        assert_eq!(
            section.get("group").unwrap().as_str(),
            Some("calibration-pair")
        );
        assert_eq!(
            section.get("wire").unwrap()["kind"].as_str(),
            Some("conductor")
        );
    }

    #[test]
    fn calibration_rejects_an_unapplied_bridge_description() {
        let setup = ofhc(30.0);
        let calibration = A1Calibration {
            cold_removal: Watts::new(3.0).unwrap(),
            other_heat: A1OtherHeat::Known(Watts::new(0.0).unwrap()),
            wire: Some(setup.bridge.clone()),
            group: None,
        };
        assert!(A1Scenario::calibrated(&setup, &calibration).is_err());
    }

    #[test]
    fn a1_preview_matches_first_thermal_step_and_horizon() {
        let preview = preview_a1(&ofhc(30.0), Watts::new(1.0).unwrap()).unwrap();
        assert_eq!(preview.len(), 31);
        assert_eq!(preview[0].time_s, 0.0);
        assert_eq!(preview[0].temperatures_k, [45.0, 15.0, 16.0, 77.0]);
        assert!((preview[0].body_k - 30.0).abs() < 1e-9);
        assert_eq!(preview[1].time_s, 0.1);
        for (actual, expected) in preview[1]
            .temperatures_k
            .into_iter()
            .zip([44.965, 14.99, 15.9956, 77.0])
        {
            assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
        }
        assert!((preview[1].bridge_w - 0.28).abs() < 1e-9);
        assert!((preview[1].heater_w - 0.28).abs() < 1e-9);
        assert!(preview[1].body_k < preview[0].body_k);
        assert_eq!(preview[30].time_s, 3.0);
    }

    #[test]
    fn a1_preview_retains_signed_bridge_flow_and_caps_the_ideal_heater() {
        let cold = preview_a1(&ofhc(10.0), Watts::new(1.0).unwrap()).unwrap();
        assert!((cold[1].bridge_w + 0.12).abs() < 1e-9);
        assert_eq!(cold[1].heater_w, 0.0);
        assert!(cold[1].body_k > cold[0].body_k);

        let capped = preview_a1(&ofhc(30.0), Watts::new(0.1).unwrap()).unwrap();
        assert!((capped[1].bridge_w - 0.28).abs() < 1e-9);
        assert_eq!(capped[1].heater_w, 0.1);
        assert!(capped
            .iter()
            .all(|point| (0.0..=0.1).contains(&point.heater_w)));
        assert!(preview_a1(&ofhc(30.0), Watts::new(-1.0).unwrap()).is_err());
    }
}
