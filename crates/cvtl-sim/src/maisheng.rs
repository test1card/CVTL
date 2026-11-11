// FILE: crates/cvtl-sim/src/maisheng.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Exercise SourceChannel against an ideal test-only resistive load
//   SCOPE: In-memory A/B state, explicit synthetic limits and deadlines
//   DEPENDS: M-CVTL-CORE, M-CVTL-INSTRUMENTS
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   MaishengSim - in-memory test-only source with independent A/B loads
// END_MODULE_MAP
//! Synthetic interface fixture. MP3001CR2 protocol and specifications remain unverified.

use std::time::Instant;

use cvtl_core::units::{Amperes, Ohms, Volts};
use cvtl_instruments::{
    keithley::{Channel, ChannelConfig, Measurement, OutputState, SourceChannel},
    InstrumentError, Result,
};

struct Branch {
    resistance: Ohms,
    config: Option<ChannelConfig>,
    current: Amperes,
    on: bool,
}

pub struct MaishengSim {
    branches: [Branch; 2],
}

impl MaishengSim {
    pub fn new_test_only(loads: [Ohms; 2]) -> Result<Self> {
        if loads.iter().any(|load| load.get() <= 0.0) {
            return Err(InstrumentError::InvalidInput(
                "Синтетическое сопротивление должно быть положительным",
            ));
        }
        Ok(Self {
            branches: loads.map(|resistance| Branch {
                resistance,
                config: None,
                current: zero_current(),
                on: false,
            }),
        })
    }

    pub fn set_resistance(&mut self, channel: Channel, resistance: Ohms) -> Result<()> {
        if resistance.get() <= 0.0 {
            return Err(InstrumentError::InvalidInput(
                "Синтетическое сопротивление должно быть положительным",
            ));
        }
        let branch = &mut self.branches[index(channel)];
        if let Some(config) = branch.config {
            voltage_for(branch.current, resistance, config)?;
        }
        branch.resistance = resistance;
        Ok(())
    }
}

impl SourceChannel for MaishengSim {
    fn configure(
        &mut self,
        channel: Channel,
        config: ChannelConfig,
        deadline: Instant,
    ) -> Result<()> {
        check_deadline(deadline)?;
        let branch = &mut self.branches[index(channel)];
        if branch.on {
            return Err(InstrumentError::State(
                "Настройка синтетического источника требует OFF",
            ));
        }
        if [
            config.limit_i.get(),
            config.limit_v.get(),
            config.limit_p.get(),
            config.source_range_i.get(),
            config.measure_range_i.get(),
            config.measure_range_v.get(),
        ]
        .iter()
        .any(|value| *value <= 0.0)
            || !config.nplc.is_finite()
            || config.nplc <= 0.0
            || config.limit_i > config.source_range_i
            || config.limit_i > config.measure_range_i
            || config.limit_v > config.measure_range_v
        {
            return Err(InstrumentError::InvalidInput(
                "Недопустимые синтетические пределы, диапазоны или NPLC",
            ));
        }
        branch.config = Some(config);
        branch.current = zero_current();
        Ok(())
    }

    fn set_current(&mut self, channel: Channel, current: Amperes, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        let branch = &mut self.branches[index(channel)];
        voltage_for(current, branch.resistance, configured(branch)?)?;
        branch.current = current;
        Ok(())
    }

    fn measure(&mut self, channel: Channel, deadline: Instant) -> Result<Measurement> {
        let started = check_deadline(deadline)?;
        let branch = &self.branches[index(channel)];
        let config = configured(branch)?;
        let current_a = if branch.on {
            branch.current
        } else {
            zero_current()
        };
        let voltage_v = voltage_for(current_a, branch.resistance, config)?;
        Ok(Measurement {
            current_a,
            voltage_v,
            nplc: config.nplc,
            started,
            finished: Instant::now(),
        })
    }

    fn output_on(&mut self, channel: Channel, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        let branch = &mut self.branches[index(channel)];
        configured(branch)?;
        branch.on = true;
        Ok(())
    }

    fn output_off(&mut self, channel: Channel, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        let branch = &mut self.branches[index(channel)];
        branch.on = false;
        branch.current = zero_current();
        Ok(())
    }

    fn output_state(&mut self, channel: Channel, deadline: Instant) -> Result<OutputState> {
        check_deadline(deadline)?;
        Ok(if self.branches[index(channel)].on {
            OutputState::On
        } else {
            OutputState::Off
        })
    }

    fn errors(&mut self, deadline: Instant) -> Result<String> {
        check_deadline(deadline)?;
        Ok(String::new())
    }

    fn identity(&mut self, deadline: Instant) -> Result<String> {
        check_deadline(deadline)?;
        Ok("CVTL test_only Maisheng interface simulator".into())
    }
}

fn index(channel: Channel) -> usize {
    match channel {
        Channel::A => 0,
        Channel::B => 1,
    }
}

fn configured(branch: &Branch) -> Result<ChannelConfig> {
    branch.config.ok_or(InstrumentError::State(
        "Синтетический источник требует настройки",
    ))
}

fn zero_current() -> Amperes {
    Amperes::new(0.0).unwrap()
}

fn check_deadline(deadline: Instant) -> Result<Instant> {
    let now = Instant::now();
    if now >= deadline {
        return Err(InstrumentError::Timeout);
    }
    Ok(now)
}

fn voltage_for(current: Amperes, resistance: Ohms, config: ChannelConfig) -> Result<Volts> {
    let voltage = current.get() * resistance.get();
    let power = current.get() * voltage;
    if current.get() < 0.0
        || current > config.limit_i
        || !voltage.is_finite()
        || !power.is_finite()
        || voltage > config.limit_v.get()
        || power > config.limit_p.get()
    {
        return Err(InstrumentError::InvalidInput(
            "Уставка или нагрузка выходит за синтетические пределы I, U или P",
        ));
    }
    Ok(Volts::new(voltage).expect("synthetic voltage was checked"))
}
