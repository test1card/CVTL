// FILE: crates/cvtl-physics/src/scenario.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Build an initial body and bridge from a recorded description
//   SCOPE: SI parameters and source curves shared by simulation and replay
//   DEPENDS: M-CVTL-CORE, M-CVTL-MATERIALS
//   LINKS: M-CVTL-PHYSICS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   BodySetup - initial material, mass, temperature, scale and bridge
//   BridgeSetup - parameters for each existing type of bridge
// END_MODULE_MAP

use cvtl_core::units::{Kelvin, Kilograms, Meters, SquareMeters, WattsPerKelvin};
use cvtl_materials::{nist_conductivity_from_catalog, nist_heat_capacity_from_catalog};
use serde::{Deserialize, Serialize};

use crate::{material, unit, Body, Bridge, Result};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BodySetup {
    pub material: String,
    pub mass_kg: f64,
    pub temperature_k: f64,
    pub scale: f64,
    pub bridge: BridgeSetup,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeSetup {
    Constant {
        conductance_w_per_k: f64,
    },
    Conductor {
        material: String,
        area_m2: f64,
        length_m: f64,
    },
    Table {
        points: Vec<[f64; 2]>,
    },
    Series {
        contact_w_per_k: f64,
        material: String,
        area_m2: f64,
        length_m: f64,
    },
}

impl BodySetup {
    pub fn build(&self, catalog: &str, initial_tf: Kelvin) -> Result<(Body, Bridge)> {
        let cp = material(nist_heat_capacity_from_catalog(&self.material, catalog))?.curve;
        let body = Body::new(
            cp,
            unit(Kilograms::new(self.mass_kg))?,
            self.scale,
            unit(Kelvin::new(self.temperature_k))?,
        )?;
        let bridge = self.bridge.build(catalog)?;
        bridge.flow(body.temperature(), initial_tf)?;
        Ok((body, bridge))
    }
}

impl BridgeSetup {
    pub fn build(&self, catalog: &str) -> Result<Bridge> {
        Ok(match self {
            Self::Constant {
                conductance_w_per_k,
            } => Bridge::Constant(unit(WattsPerKelvin::new(*conductance_w_per_k))?),
            Self::Conductor {
                material: id,
                area_m2,
                length_m,
            } => Bridge::Conductor {
                conductivity: material(nist_conductivity_from_catalog(id, catalog))?.curve,
                area: unit(SquareMeters::new(*area_m2))?,
                length: unit(Meters::new(*length_m))?,
            },
            Self::Table { points } => Bridge::Table(
                points
                    .iter()
                    .map(|&[t, g]| Ok((unit(Kelvin::new(t))?, unit(WattsPerKelvin::new(g))?)))
                    .collect::<Result<_>>()?,
            ),
            Self::Series {
                contact_w_per_k,
                material: id,
                area_m2,
                length_m,
            } => Bridge::Series {
                contact: unit(WattsPerKelvin::new(*contact_w_per_k))?,
                conductivity: material(nist_conductivity_from_catalog(id, catalog))?.curve,
                area: unit(SquareMeters::new(*area_m2))?,
                length: unit(Meters::new(*length_m))?,
            },
        })
    }
}
