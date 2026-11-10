// FILE: crates/cvtl-daemon/src/scenario.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Validate and estimate an operator scenario
//   SCOPE: Synthetic preview, wire heat leak, fixture capacity and test limits
//   DEPENDS: M-CVTL-CORE, M-CVTL-ENVELOPE, M-CVTL-MATERIALS, M-CVTL-PHYSICS, M-CVTL-SIM, M-CVTL-SAFETY
//   LINKS: M-CVTL-DAEMON
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Setup - initial body, wire segments and fixture parts
//   Estimate - preview and preflight calculations
// END_MODULE_MAP

use cvtl_core::units::{Amperes, Kelvin, Kilograms, Meters, SquareMeters, Watts};
use cvtl_envelope::advise_pair;
use cvtl_materials::{
    lakeshore_wire_conductivity, nist_conductivity, nist_heat_capacity, LAKESHORE_WIRE_SOURCE,
};
use cvtl_physics::{
    fixture_heat_capacity,
    scenario::{BodySetup, BridgeSetup},
    wire::{solve, WireSegment},
    InventoryPart,
};
use cvtl_safety::monitor::ProtectionProfile;
use cvtl_sim::recording::{preview_a1, test_only_control_config, test_only_source_config};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Setup {
    pub body: BodySetup,
    pub wiring: Vec<Wire>,
    pub fixture: Vec<FixturePart>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Node {
    Stage1,
    Stage2,
    Tf,
    Jacket,
}

impl Node {
    fn index(self) -> usize {
        match self {
            Self::Stage1 => 0,
            Self::Stage2 => 1,
            Self::Tf => 2,
            Self::Jacket => 3,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wire {
    from: Node,
    to: Node,
    material: String,
    length_m: f64,
    area_mm2: f64,
    strands: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixturePart {
    material: String,
    mass_kg: f64,
}

impl Default for Setup {
    fn default() -> Self {
        let wire = |from, to| Wire {
            from,
            to,
            material: "lakeshore_phosphor_bronze".into(),
            length_m: 0.3,
            area_mm2: 0.12,
            strands: 2,
        };
        Self {
            body: BodySetup {
                material: "ofhc_rrr100".into(),
                mass_kg: 20.0,
                temperature_k: 30.0,
                scale: 0.1,
                bridge: BridgeSetup::Constant {
                    conductance_w_per_k: 0.02,
                },
            },
            wiring: vec![
                wire(Node::Jacket, Node::Stage1),
                wire(Node::Stage1, Node::Stage2),
            ],
            fixture: vec![FixturePart {
                material: "ofhc_rrr100".into(),
                mass_kg: 0.2,
            }],
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct Estimate {
    pub preview: Vec<PreviewPoint>,
    pub wiring: Vec<WireEstimate>,
    pub wire_heat_by_node_w: [f64; 4],
    pub fixture_capacity_j_per_k: f64,
    pub limits: Limits,
}

#[derive(Clone, Serialize)]
pub(crate) struct PreviewPoint {
    time_s: f64,
    body_k: f64,
    temperatures_k: [f64; 4],
    bridge_w: f64,
    heater_w: f64,
}

#[derive(Clone, Serialize)]
pub(crate) struct WireEstimate {
    from: Node,
    to: Node,
    heat_from_w: f64,
    heat_to_w: f64,
    source: &'static str,
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Limits {
    envelope_source_w: [f64; 2],
    envelope_heater_w: [f64; 2],
    envelope_total_w: f64,
    source_power_w: f64,
    source_current_a: f64,
    source_voltage_v: f64,
    protection_total_w: f64,
    protection_branch_w: [f64; 2],
    protection_current_a: [f64; 2],
    protection_voltage_v: [f64; 2],
    inversion_k: [f64; 3],
    temperature_k: [f64; 3],
    min_temperature_k: f64,
    max_rate_k_per_s: f64,
    pub max_temperature_age_ms: u64,
}

impl Setup {
    pub(crate) fn estimate(&self) -> Result<Estimate, String> {
        let limits = limits()?;
        let cap = limits
            .envelope_total_w
            .min(2.0 * limits.source_power_w)
            .min(limits.protection_total_w);
        let preview = preview_a1(&self.body, finite(Watts::new(cap))?)
            .map_err(|error| format!("Прогноз A1 недоступен ({error})"))?;
        let temperatures = preview[0].temperatures_k;
        let wiring = self
            .wiring
            .iter()
            .map(|wire| wire.estimate(temperatures))
            .collect::<Result<Vec<_>, _>>()?;
        let mut heat = [0.0; 4];
        for wire in &wiring {
            heat[wire.from.index()] += wire.heat_from_w;
            heat[wire.to.index()] += wire.heat_to_w;
        }
        if heat.iter().any(|value| !value.is_finite()) {
            return Err("Суммарный поток проводки вне численного диапазона".into());
        }
        let tf = finite(Kelvin::new(temperatures[Node::Tf.index()]))?;
        let inventory = self
            .fixture
            .iter()
            .map(|part| {
                let cp = nist_heat_capacity(&part.material).map_err(|_| {
                    format!("Для материала насадки {} нет теплоёмкости", part.material)
                })?;
                Ok(InventoryPart {
                    mass: finite(Kilograms::new(part.mass_kg))?,
                    temperature: tf,
                    heat_capacity: cp.curve,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let capacity = fixture_heat_capacity(&inventory)
            .map_err(|error| format!("Расчёт теплоёмкости насадки недоступен ({error})"))?;
        Ok(Estimate {
            preview: preview
                .into_iter()
                .map(|point| PreviewPoint {
                    time_s: point.time_s,
                    body_k: point.body_k,
                    temperatures_k: point.temperatures_k,
                    bridge_w: point.bridge_w,
                    heater_w: point.heater_w,
                })
                .collect(),
            wiring,
            wire_heat_by_node_w: heat,
            fixture_capacity_j_per_k: capacity.get(),
            limits,
        })
    }
}

impl Wire {
    fn estimate(&self, temperatures: [f64; 4]) -> Result<WireEstimate, String> {
        if self.from == self.to || self.strands == 0 {
            return Err("Для участка проводки нужны разные узлы и ненулевое число жил".into());
        }
        let missing = |_| {
            format!(
                "Для материала проводки {} нет теплопроводности",
                self.material
            )
        };
        let (conductivity, source) = if self.material.starts_with("lakeshore_") {
            (
                lakeshore_wire_conductivity(&self.material).map_err(missing)?,
                LAKESHORE_WIRE_SOURCE,
            )
        } else {
            let curve = nist_conductivity(&self.material).map_err(missing)?;
            (curve.curve, curve.provenance.source_url)
        };
        let result = solve(&WireSegment {
            conductivity,
            resistivity: None,
            length_m: finite(Meters::new(self.length_m))?,
            area_m2: finite(SquareMeters::new(self.area_mm2 * 1e-6))?,
            current_a: Amperes::new(0.0).unwrap(),
            start_k: finite(Kelvin::new(temperatures[self.from.index()]))?,
            end_k: finite(Kelvin::new(temperatures[self.to.index()]))?,
            radiation: None,
        })
        .map_err(|error| format!("Расчёт участка проводки недоступен ({error:?})"))?;
        let strands = f64::from(self.strands);
        Ok(WireEstimate {
            from: self.from,
            to: self.to,
            heat_from_w: finite(Watts::new(strands * result.heat_into_start_w.get()))?.get(),
            heat_to_w: finite(Watts::new(strands * result.heat_into_end_w.get()))?.get(),
            source,
        })
    }
}

fn limits() -> Result<Limits, String> {
    let source = test_only_source_config();
    let control = test_only_control_config();
    let protection = ProtectionProfile::test_only_example();
    let [a, b] = control.branches;
    let advice = advise_pair(a, b, control.margins, Some(control.total_limit))
        .map_err(|_| "Расчёт области источника недоступен")?;
    let capped = |value: Option<cvtl_envelope::Point>| {
        value
            .map(|point| point.power.get())
            .ok_or("Для эмулятора требуется предел нагревателя")
    };
    Ok(Limits {
        envelope_source_w: [advice.a.source.power.get(), advice.b.source.power.get()],
        envelope_heater_w: [capped(advice.a.capped)?, capped(advice.b.capped)?],
        envelope_total_w: advice
            .capped_sum
            .ok_or("Для эмулятора требуется общий предел мощности")?
            .get(),
        source_power_w: source.limit_p.get(),
        source_current_a: source.limit_i.get(),
        source_voltage_v: source.limit_v.get(),
        protection_total_w: protection.max_total_power_w.get(),
        protection_branch_w: protection.max_branch_power_w.map(Watts::get),
        protection_current_a: protection.max_current_a.map(Amperes::get),
        protection_voltage_v: protection.max_voltage_v.map(|value| value.get()),
        inversion_k: [
            protection.head.inversion_warning_k.get(),
            protection.head.inversion_derate_k.get(),
            protection.head.inversion_trip_k.get(),
        ],
        temperature_k: [
            protection.head.temperature_warning_k.get(),
            protection.head.temperature_derate_k.get(),
            protection.head.temperature_trip_k.get(),
        ],
        min_temperature_k: protection.head.min_temperature_k.get(),
        max_rate_k_per_s: protection.head.max_rate_k_per_s.get(),
        max_temperature_age_ms: protection.max_temperature_age.as_millis() as u64,
    })
}

fn finite<T>(value: Option<T>) -> Result<T, String> {
    value.ok_or_else(|| "Значение вне численного диапазона".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_setup_uses_a1_initial_values_and_existing_calculations() {
        let setup = Setup::default();
        let estimate = setup.estimate().unwrap();
        assert_eq!(estimate.preview.len(), 31);
        assert_eq!(estimate.preview[0].temperatures_k, [45.0, 15.0, 16.0, 77.0]);
        assert!((estimate.preview[0].body_k - 30.0).abs() < 1e-9);
        let first = &estimate.preview[1];
        assert!((first.time_s - 0.1).abs() < 1e-9);
        for (actual, expected) in first
            .temperatures_k
            .into_iter()
            .zip([44.965, 14.99, 15.9956, 77.0])
        {
            assert!((actual - expected).abs() < 1e-9);
        }
        assert!((estimate.preview[0].bridge_w - 0.28).abs() < 1e-9);
        assert!(estimate.wiring[1].heat_to_w > 0.0);
        assert!(estimate.fixture_capacity_j_per_k > 0.0);
        assert_eq!(estimate.limits.source_power_w, 4.0);
        assert_eq!(estimate.limits.inversion_k, [-1.0, 0.0, 1.0]);
    }

    #[test]
    fn rejects_invalid_setup_diagnostics() {
        let mut setup = Setup::default();
        setup.wiring[0].strands = 0;
        assert!(setup.estimate().is_err());
        let mut setup = Setup::default();
        setup.fixture[0].mass_kg = 0.0;
        assert!(setup.estimate().is_err());
    }
}
