//! Pure power-space controller. Sample freshness must be checked by the caller.
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts};

#[derive(Clone, Copy, Debug)]
pub struct DerivativeConfig {
    pub kd: Seconds,
    pub tau: Seconds,
}
#[derive(Clone, Copy, Debug)]
pub struct PiConfig {
    pub kp: f64,
    pub ki: PerSecond,
    pub integral_limit: Watts,
    pub resistance_initial: Ohms,
    pub resistance_min: Ohms,
    pub resistance_max: Ohms,
    pub resistance_tau: Seconds,
    pub min_current: Amperes,
    pub current_quantum: Option<Amperes>,
    pub derivative: Option<DerivativeConfig>,
}
#[derive(Clone, Copy, Debug)]
pub struct PowerSample {
    pub voltage: Volts,
    pub current: Amperes,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiError {
    InvalidConfig,
    InvalidInput,
    MissingMeasurement,
    Arithmetic,
}
#[derive(Clone, Copy, Debug)]
pub struct PiOutput {
    pub current: Amperes,
    pub measured_power: Option<Watts>,
    pub resistance: Ohms,
    pub saturated: bool,
}
#[derive(Clone, Debug)]
pub struct Pi {
    config: PiConfig,
    resistance: f64,
    integral: f64,
    previous_power: Option<f64>,
    derivative: f64,
}
impl Pi {
    pub fn new(config: PiConfig) -> Result<Self, PiError> {
        let c = config;
        if !c.kp.is_finite()
            || c.kp < 0.0
            || c.ki.get() < 0.0
            || c.integral_limit.get() < 0.0
            || c.resistance_min.get() <= 0.0
            || c.resistance_max.get() < c.resistance_min.get()
            || !(c.resistance_min.get()..=c.resistance_max.get())
                .contains(&c.resistance_initial.get())
            || c.resistance_tau.get() < 0.0
            || c.min_current.get() <= 0.0
            || c.current_quantum.is_some_and(|q| q.get() <= 0.0)
            || c.derivative
                .is_some_and(|d| d.kd.get() < 0.0 || d.tau.get() <= 0.0)
        {
            return Err(PiError::InvalidConfig);
        }
        Ok(Self {
            config,
            resistance: c.resistance_initial.get(),
            integral: 0.0,
            previous_power: None,
            derivative: 0.0,
        })
    }
    pub fn reset(&mut self) {
        self.resistance = self.config.resistance_initial.get();
        self.integral = 0.0;
        self.previous_power = None;
        self.derivative = 0.0;
    }
    fn power(sample: Option<PowerSample>) -> Result<f64, PiError> {
        let s = sample.ok_or(PiError::MissingMeasurement)?;
        if s.voltage.get() < 0.0 || s.current.get() < 0.0 {
            return Err(PiError::InvalidInput);
        }
        let p = s.voltage.get() * s.current.get();
        if !p.is_finite() {
            return Err(PiError::Arithmetic);
        }
        Ok(p)
    }
    /// Rebase against actual current, clamping the integral to its configured bounds.
    /// Exact output continuity requires that the ideal integral lies within those bounds.
    pub fn rebase(
        &mut self,
        target: Watts,
        measurement: Option<PowerSample>,
        actual_current: Amperes,
    ) -> Result<(), PiError> {
        let p = Self::power(measurement)?;
        if target.get() < 0.0 || actual_current.get() < 0.0 {
            return Err(PiError::InvalidInput);
        }
        let integral = actual_current.get() * actual_current.get() * self.resistance
            - target.get()
            - self.config.kp * (target.get() - p);
        if !integral.is_finite() {
            return Err(PiError::Arithmetic);
        }
        let limit = self.config.integral_limit.get();
        self.integral = integral.clamp(-limit, limit);
        self.previous_power = Some(p);
        self.derivative = 0.0;
        Ok(())
    }
    pub fn step(
        &mut self,
        target: Watts,
        measurement: Option<PowerSample>,
        dt: Seconds,
        max_current: Amperes,
    ) -> Result<PiOutput, PiError> {
        let p = Self::power(measurement)?;
        let s = measurement.ok_or(PiError::MissingMeasurement)?;
        if target.get() < 0.0 || dt.get() <= 0.0 || max_current.get() < 0.0 {
            return Err(PiError::InvalidInput);
        }
        let mut resistance = self.resistance;
        if s.current.get() >= self.config.min_current.get() {
            let observed = s.voltage.get() / s.current.get();
            if !observed.is_finite() {
                return Err(PiError::Arithmetic);
            }
            let observed = observed.clamp(
                self.config.resistance_min.get(),
                self.config.resistance_max.get(),
            );
            let alpha = if self.config.resistance_tau.get() == 0.0 {
                1.0
            } else {
                -(-dt.get() / self.config.resistance_tau.get()).exp_m1()
            };
            resistance += alpha * (observed - resistance);
        }
        let mut derivative = self.derivative;
        if let Some(d) = self.config.derivative {
            if let Some(previous) = self.previous_power {
                let alpha = -(-dt.get() / d.tau.get()).exp_m1();
                derivative += alpha * ((p - previous) / dt.get() - derivative);
            }
        }
        let error = target.get() - p;
        let proportional = target.get() + self.config.kp * error
            - self
                .config
                .derivative
                .map_or(0.0, |d| d.kd.get() * derivative);
        let limit = self.config.integral_limit.get();
        let candidate = self.integral + self.config.ki.get() * error * dt.get();
        if !candidate.is_finite() {
            return Err(PiError::Arithmetic);
        }
        let proposed = candidate.clamp(-limit, limit);
        let max_power = max_current.get() * max_current.get() * resistance;
        if !resistance.is_finite()
            || !derivative.is_finite()
            || !error.is_finite()
            || !proportional.is_finite()
            || !proposed.is_finite()
            || !max_power.is_finite()
        {
            return Err(PiError::Arithmetic);
        }
        let proposed_power = proportional + proposed;
        let integral = if (proposed_power > max_power && error > 0.0)
            || (proposed_power < 0.0 && error < 0.0)
        {
            self.integral
        } else {
            proposed
        };
        let power = proportional + integral;
        if !power.is_finite() {
            return Err(PiError::Arithmetic);
        }
        let raw = (power.max(0.0) / resistance).sqrt();
        if !raw.is_finite() {
            return Err(PiError::Arithmetic);
        }
        let mut current = raw.min(max_current.get());
        if let Some(q) = self.config.current_quantum {
            let steps = current / q.get();
            if !steps.is_finite() {
                return Err(PiError::Arithmetic);
            }
            let mut grid = steps.floor();
            current = grid * q.get();
            if current > raw.min(max_current.get()) {
                grid = if grid > 0.0 {
                    f64::from_bits(grid.to_bits() - 1).floor()
                } else {
                    0.0
                };
                current = grid * q.get();
            }
            if current > raw.min(max_current.get()) {
                return Err(PiError::Arithmetic);
            }
        }
        let output = PiOutput {
            current: Amperes::new(current).ok_or(PiError::Arithmetic)?,
            measured_power: Some(Watts::new(p).ok_or(PiError::Arithmetic)?),
            resistance: Ohms::new(resistance).ok_or(PiError::Arithmetic)?,
            saturated: raw > max_current.get(),
        };
        self.resistance = resistance;
        self.integral = integral;
        self.previous_power = Some(p);
        self.derivative = derivative;
        Ok(output)
    }
}
