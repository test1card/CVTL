//! Deterministic, test-only plant DTO. Tick delays are virtual, not host timing.
//! Public inputs and outputs use typed SI quantities.
//! Values must be finite; configurations and resistance updates are validated.
//! The plant is not an instrument or a safety boundary.
use cvtl_core::units::{Amperes, Ohms, Watts};
use std::collections::VecDeque;

/// Bound test allocations independently of caller-supplied configuration.
pub const MAX_DELAY_TICKS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Branch {
    pub resistance_ohm: Ohms,
    pub current_step_a: Amperes,
}

#[derive(Clone, Copy, Debug)]
pub struct PlantConfig {
    pub branches: [Branch; 2],
    pub command_delay_ticks: usize,
    pub measurement_delay_ticks: usize,
    pub noise_amplitude_w: Watts,
    pub noise_seed: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PlantSample {
    pub actual_power_w: [Watts; 2],
    pub measured_power_w: [Watts; 2],
    pub measured_current_a: [Amperes; 2],
    /// Zero-based plant tick at which this measurement was generated; None is priming.
    pub measurement_tick: Option<u64>,
}

pub struct Plant {
    config: PlantConfig,
    commands: VecDeque<[f64; 2]>,
    measurements: VecDeque<([f64; 2], [f64; 2], Option<u64>)>,
    tick_index: u64,
    failed: [bool; 2],
    rng: u64,
}

impl Plant {
    pub fn new(config: PlantConfig) -> Result<Self, &'static str> {
        if config
            .branches
            .iter()
            .any(|b| b.resistance_ohm.get() <= 0.0 || b.current_step_a.get() <= 0.0)
            || config.noise_amplitude_w.get() < 0.0
            || config.command_delay_ticks > MAX_DELAY_TICKS
            || config.measurement_delay_ticks > MAX_DELAY_TICKS
        {
            return Err("invalid plant configuration");
        }
        Ok(Self {
            commands: VecDeque::from(vec![[0.0; 2]; config.command_delay_ticks]),
            measurements: VecDeque::from(vec![
                ([0.0; 2], [0.0; 2], None);
                config.measurement_delay_ticks
            ]),
            failed: [false; 2],
            tick_index: 0,
            rng: config.noise_seed,
            config,
        })
    }

    pub fn set_resistance(
        &mut self,
        branch: usize,
        resistance_ohm: Ohms,
    ) -> Result<(), &'static str> {
        if branch >= 2 || resistance_ohm.get() <= 0.0 {
            return Err("invalid resistance");
        }
        self.config.branches[branch].resistance_ohm = resistance_ohm;
        Ok(())
    }

    pub fn set_failed(&mut self, branch: usize, failed: bool) -> Result<(), &'static str> {
        if branch >= 2 {
            return Err("invalid branch");
        }
        self.failed[branch] = failed;
        Ok(())
    }

    pub fn tick(&mut self, command_a: [Amperes; 2]) -> Result<PlantSample, &'static str> {
        if command_a.iter().any(|x| x.get() < 0.0) {
            return Err("negative command");
        }
        let queued = command_a.map(|x| x.get());
        let command = self.commands.front().copied().unwrap_or(queued);
        let mut rng = self.rng;
        let mut actual = [0.0; 2];
        let mut measured = [0.0; 2];
        let mut currents = [0.0; 2];
        for i in 0..2 {
            let branch = self.config.branches[i];
            let current = if self.failed[i] {
                0.0
            } else {
                (command[i] / branch.current_step_a.get()).round() * branch.current_step_a.get()
            };
            currents[i] = current;
            actual[i] = current * current * branch.resistance_ohm.get();
            // Deterministic LCG; bounded additive noise in watts.
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let uniform = (rng >> 11) as f64 / ((1_u64 << 53) as f64);
            measured[i] = if current == 0.0 {
                0.0
            } else {
                (actual[i] + (2.0 * uniform - 1.0) * self.config.noise_amplitude_w.get()).max(0.0)
            };
        }
        if actual
            .iter()
            .chain(measured.iter())
            .chain(currents.iter())
            .any(|x| !x.is_finite())
        {
            return Err("plant arithmetic overflow");
        }
        self.commands.push_back(queued);
        self.commands.pop_front();
        self.rng = rng;
        self.measurements
            .push_back((measured, currents, Some(self.tick_index)));
        self.tick_index = self.tick_index.saturating_add(1);
        let (measured_power_w, measured_current_a, measurement_tick) = self
            .measurements
            .pop_front()
            .expect("measurement was just queued");
        Ok(PlantSample {
            actual_power_w: actual.map(|v| Watts::new(v).expect("checked finite")),
            measured_power_w: measured_power_w.map(|v| Watts::new(v).expect("checked finite")),
            measured_current_a: measured_current_a
                .map(|v| Amperes::new(v).expect("checked finite")),
            measurement_tick,
        })
    }
}
