// FILE: crates/cvtl-sim/src/head.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Advance the synthetic A1 head
//   SCOPE: Three-node explicit energy balance with typed SI inputs
//   DEPENDS: M-CVTL-CORE
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   SyntheticHead - validated test-only thermal state
//   HeadParameters - capacities and internal conductances
//   HeadTemperatures - stage1, stage2 and tf
//   HeadError - invalid model or atomic step rejection
// END_MODULE_MAP
//! Synthetic test-only three-node head. Cooling power is a scenario input.

use std::fmt;

use cvtl_core::units::{JoulesPerKelvin, Kelvin, Seconds, Watts, WattsPerKelvin};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadParameters {
    pub stage1_capacity: JoulesPerKelvin,
    pub stage2_capacity: JoulesPerKelvin,
    pub fixture_capacity: JoulesPerKelvin,
    pub stage_link: WattsPerKelvin,
    pub fixture_link: WattsPerKelvin,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadTemperatures {
    pub stage1: Kelvin,
    pub stage2: Kelvin,
    pub tf: Kelvin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadError {
    Parameters,
    Step,
    Power,
    Temperature,
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Parameters => "Недопустимая теплоёмкость или проводимость модели головки",
            Self::Step => "Недопустимый шаг синтетической головки",
            Self::Power => "Мощность нагрева и величина отвода должны быть неотрицательными",
            Self::Temperature => "Температура модели ниже 0 К или не конечна",
        })
    }
}

impl std::error::Error for HeadError {}

pub struct SyntheticHead {
    parameters: HeadParameters,
    temperatures: HeadTemperatures,
}

impl SyntheticHead {
    pub fn new(
        parameters: HeadParameters,
        temperatures: HeadTemperatures,
    ) -> Result<Self, HeadError> {
        if parameters.stage1_capacity.get() <= 0.0
            || parameters.stage2_capacity.get() <= 0.0
            || parameters.fixture_capacity.get() <= 0.0
            || parameters.stage_link.get() < 0.0
            || parameters.fixture_link.get() < 0.0
        {
            return Err(HeadError::Parameters);
        }
        if [temperatures.stage1, temperatures.stage2, temperatures.tf]
            .iter()
            .any(|t| t.get() < 0.0)
        {
            return Err(HeadError::Temperature);
        }
        Ok(Self {
            parameters,
            temperatures,
        })
    }

    pub fn temperatures(&self) -> HeadTemperatures {
        self.temperatures
    }

    /// Heat delivered by A/B enters tf. Positive cooling is removed from stage1/stage2.
    /// All exchanges use the temperatures at the start of the interval.
    pub fn advance(
        &mut self,
        dt: Seconds,
        heater: [Watts; 2],
        cooling: [Watts; 2],
    ) -> Result<(), HeadError> {
        self.advance_inner(dt, heater, cooling, None)
    }

    pub(super) fn advance_with_fixture(
        &mut self,
        dt: Seconds,
        heater: [Watts; 2],
        cooling: [Watts; 2],
        capacity: JoulesPerKelvin,
        parasitic: Watts,
    ) -> Result<(), HeadError> {
        if capacity.get() <= 0.0 {
            return Err(HeadError::Parameters);
        }
        if parasitic.get() < 0.0 {
            return Err(HeadError::Power);
        }
        self.advance_inner(dt, heater, cooling, Some((capacity, parasitic)))
    }

    fn advance_inner(
        &mut self,
        dt: Seconds,
        heater: [Watts; 2],
        cooling: [Watts; 2],
        fixture: Option<(JoulesPerKelvin, Watts)>,
    ) -> Result<(), HeadError> {
        let p = self.parameters;
        let [c1, c2, cf] = [
            p.stage1_capacity.get(),
            p.stage2_capacity.get(),
            fixture
                .map_or(p.fixture_capacity, |(capacity, _)| capacity)
                .get(),
        ];
        let g12 = p.stage_link.get();
        let gf2 = p.fixture_link.get();
        let dt = dt.get();
        if dt <= 0.0
            || [g12, g12 + gf2, gf2]
                .into_iter()
                .zip([c1, c2, cf])
                .any(|(g, c)| dt * g > c)
        {
            return Err(HeadError::Step);
        }
        if heater.iter().chain(&cooling).any(|power| power.get() < 0.0) {
            return Err(HeadError::Power);
        }

        let t = self.temperatures;
        let q12 = g12 * (t.stage1.get() - t.stage2.get());
        let qf2 = gf2 * (t.tf.get() - t.stage2.get());
        let fixture_power = heater[0].get() + heater[1].get() - qf2;
        let fixture_power = match fixture {
            Some((_, parasitic)) => fixture_power + parasitic.get(),
            None => fixture_power,
        };
        let next = HeadTemperatures {
            stage1: step(t.stage1, -cooling[0].get() - q12, dt, c1)?,
            stage2: step(t.stage2, -cooling[1].get() + q12 + qf2, dt, c2)?,
            tf: step(t.tf, fixture_power, dt, cf)?,
        };
        self.temperatures = next;
        Ok(())
    }
}

fn step(temperature: Kelvin, power: f64, dt: f64, capacity: f64) -> Result<Kelvin, HeadError> {
    Kelvin::new(temperature.get() + power * dt / capacity)
        .filter(|next| next.get() >= 0.0)
        .ok_or(HeadError::Temperature)
}
