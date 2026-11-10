//! M4 sequencing. The caller supplies the heat permit and applies the returned
//! action. This module owns no instrument connection or physical safety state.

use crate::{
    allocation::Allocator,
    pi::{Pi, PiConfig, PowerSample},
};
use cvtl_core::units::{Amperes, Kelvin, Seconds, Watts, WattsPerKelvin, WattsPerKelvinSecond};
use cvtl_envelope::{advise, check_current, ChannelSpec, Margins, ResistanceRange};

#[derive(Clone, Copy, Debug)]
pub struct TemperatureHoldConfig {
    pub kp: WattsPerKelvin,
    pub ki: WattsPerKelvinSecond,
}

#[derive(Clone, Copy, Debug)]
pub struct ControlConfig {
    pub pi: [PiConfig; 2],
    pub branches: [ChannelSpec; 2],
    pub margins: Margins,
    pub total_limit: Watts,
    pub max_sample_age: Seconds,
    pub temperature_hold: Option<TemperatureHoldConfig>,
}

#[derive(Clone, Copy, Debug)]
pub enum Demand {
    Observe,
    Replay,
    Power(Watts),
    PowerProfile(Watts),
    VirtualBody(Watts),
    TemperatureHold {
        setpoint: Kelvin,
        stage1: Kelvin,
        stage2: Kelvin,
        at: Seconds,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum HeatPermit {
    Inhibit,
    Limit(Watts),
}

#[derive(Clone, Copy, Debug)]
pub struct TimedPowerSample {
    pub at: Seconds,
    pub sample: PowerSample,
}

#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub demand: Demand,
    pub permit: HeatPermit,
    pub branches_available: [bool; 2],
    pub measurements: [Option<TimedPowerSample>; 2],
    pub now: Seconds,
    pub dt: Seconds,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Actuation {
    /// The caller must request output OFF on both branches.
    Off,
    Drive([Amperes; 2]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    BranchLost,
    MeasurementUnavailable,
    MeasurementStale,
    TemperatureInterlock,
    Clock,
    Calculation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// A single stale tick held the previous safe command without PI updates.
    MeasurementStale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Equivalence {
    Inactive,
    Inhibited,
    Fault,
    Limited,
    Settling,
    WithinTolerance,
}

#[derive(Clone, Copy, Debug)]
pub struct ControlOutput {
    pub requested: Option<Watts>,
    pub limited: Watts,
    pub measured: Option<Watts>,
    pub targets: [Watts; 2],
    pub action: Actuation,
    pub equivalence: Equivalence,
    pub fault: Option<Fault>,
    pub warning: Option<Warning>,
    pub slew_limited: bool,
    pub power_limited: bool,
    pub current_limited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    InvalidConfig,
}

#[derive(Clone, Copy, PartialEq)]
enum ActiveMode {
    Power(Watts),
    Profile,
    Virtual,
    Temperature(Kelvin),
}

#[derive(Clone)]
pub struct Controller {
    config: ControlConfig,
    source_currents: [Amperes; 2],
    pi: [Pi; 2],
    allocator: Allocator,
    fault: Option<Fault>,
    mode: Option<ActiveMode>,
    previous_current: [Amperes; 2],
    previous_targets: [Watts; 2],
    last_tick: Option<Seconds>,
    last_sample: [Option<Seconds>; 2],
    stale_held: bool,
    temperature_integral: f64,
}

fn zero_watts() -> Watts {
    Watts::new(0.0).unwrap()
}

fn zero_current() -> Amperes {
    Amperes::new(0.0).unwrap()
}

fn power(current: Amperes, resistance: f64) -> f64 {
    current.get() * (current.get() * resistance)
}

fn upper_power(current: Amperes, resistance: f64) -> f64 {
    let i = current.get();
    if i == 0.0 {
        0.0
    } else {
        (i * (i * resistance).next_up()).next_up()
    }
}

impl Controller {
    pub fn new(config: ControlConfig, allocator: Allocator) -> Result<Self, ControlError> {
        let bad = ControlError::InvalidConfig;
        if config.total_limit.get() <= 0.0
            || config.max_sample_age.get() <= 0.0
            || config
                .temperature_hold
                .is_some_and(|c| c.kp.get() < 0.0 || c.ki.get() < 0.0)
        {
            return Err(bad);
        }
        let mut source_currents = [zero_current(); 2];
        for (i, current) in source_currents.iter_mut().enumerate() {
            let branch = config.branches[i];
            let (min, max) = branch.resistance.bounds();
            if config.pi[i].resistance_min != min || config.pi[i].resistance_max != max {
                return Err(bad);
            }
            // For nonnegative current both force voltage and heater power
            // attain their maximum at the upper end of the resistance interval.
            let worst = ChannelSpec {
                resistance: ResistanceRange::new(max, max).map_err(|_| bad)?,
                ..branch
            };
            let advice = advise(worst, config.margins).map_err(|_| bad)?;
            *current = advice.capped.ok_or(bad)?.current;
        }
        Ok(Self {
            config,
            source_currents,
            pi: [
                Pi::new(config.pi[0]).map_err(|_| bad)?,
                Pi::new(config.pi[1]).map_err(|_| bad)?,
            ],
            allocator,
            fault: None,
            mode: None,
            previous_current: [zero_current(); 2],
            previous_targets: [zero_watts(); 2],
            last_tick: None,
            last_sample: [None; 2],
            stale_held: false,
            temperature_integral: 0.0,
        })
    }

    /// Clears the M4 fault latch and controller history. The next tick still
    /// needs a fresh sample and a permit to produce an active action.
    pub fn reset_fault(&mut self) {
        self.fault = None;
        self.clear_output_history();
    }

    pub fn tick(&mut self, tick: Tick) -> Result<ControlOutput, ControlError> {
        let requested = match tick.demand {
            Demand::Power(p) | Demand::PowerProfile(p) | Demand::VirtualBody(p) => Some(p),
            _ => None,
        };
        let dt = tick.dt.get();
        if dt <= 0.0
            || tick.now.get() < 0.0
            || self.last_tick.is_some_and(|previous| {
                let elapsed = tick.now.get() - previous.get();
                elapsed <= 0.0 || (elapsed - dt).abs() > 1e-9 * dt.max(1.0)
            })
        {
            return Ok(self.latch(Fault::Clock, requested, None));
        }
        self.last_tick = Some(tick.now);
        let ordered = tick.measurements.iter().enumerate().all(|(i, sample)| {
            sample.is_some_and(|sample| {
                sample.at.get() >= 0.0
                    && sample.at <= tick.now
                    && self.last_sample[i].is_none_or(|previous| previous <= sample.at)
            })
        });
        let fresh = ordered
            && tick
                .measurements
                .iter()
                .flatten()
                .all(|s| self.is_fresh(s.at, tick.now));
        let sample_power = Self::total_measured(tick.measurements);
        let measured = fresh.then_some(sample_power).flatten();
        if self.fault.is_some() {
            return Ok(self.off(requested, measured, Equivalence::Fault));
        }
        if matches!(tick.demand, Demand::Observe | Demand::Replay) {
            return Ok(self.off(None, measured, Equivalence::Inactive));
        }
        if tick.branches_available.contains(&false) {
            return Ok(self.latch(Fault::BranchLost, requested, measured));
        }
        let cap = match tick.permit {
            HeatPermit::Inhibit => {
                return Ok(self.off(requested, measured, Equivalence::Inhibited))
            }
            HeatPermit::Limit(p) if p.get() == 0.0 => {
                return Ok(self.off(requested, measured, Equivalence::Inhibited));
            }
            HeatPermit::Limit(p) if p.get() > 0.0 => p.get().min(self.config.total_limit.get()),
            HeatPermit::Limit(_) => return Ok(self.latch(Fault::Calculation, requested, measured)),
        };
        let [Some(a), Some(b)] = tick.measurements else {
            return Ok(self.latch(Fault::MeasurementUnavailable, requested, None));
        };
        let samples = [a, b];
        if sample_power.is_none() {
            return Ok(self.latch(Fault::MeasurementUnavailable, requested, None));
        }
        if let Demand::TemperatureHold {
            setpoint,
            stage1,
            stage2,
            at,
        } = tick.demand
        {
            if self.config.temperature_hold.is_none() || setpoint.get() <= 0.0 {
                return Ok(self.off(requested, measured, Equivalence::Inhibited));
            }
            if !self.is_fresh(at, tick.now) || stage2.get() <= 0.0 || stage2 >= stage1 {
                return Ok(self.latch(Fault::TemperatureInterlock, requested, measured));
            }
        }
        if !fresh {
            let max_age = self.config.max_sample_age.get();
            let grace = max_age + dt.min(max_age);
            let one_tick = ordered
                && samples
                    .iter()
                    .all(|sample| tick.now.get() - sample.at.get() <= grace + 1e-9);
            if self.mode.is_some() && !self.stale_held && one_tick {
                return Ok(match self.hold_stale(tick, cap, requested) {
                    Some(output) => output,
                    None => self.latch(Fault::Calculation, requested, None),
                });
            }
            return Ok(self.latch(Fault::MeasurementStale, requested, None));
        }
        let mut next = self.clone();
        next.stale_held = false;
        match next.drive(tick, cap, samples, measured) {
            Ok(output) => {
                *self = next;
                Ok(output)
            }
            Err(fault) => Ok(self.latch(fault, requested, measured)),
        }
    }

    fn hold_stale(
        &mut self,
        tick: Tick,
        cap: f64,
        requested: Option<Watts>,
    ) -> Option<ControlOutput> {
        if requested.is_some_and(|request| request.get() <= 0.0) {
            let mut output = self.off(requested, None, Equivalence::Limited);
            output.warning = Some(Warning::MeasurementStale);
            output.power_limited = requested.is_some_and(|request| request.get() < 0.0);
            return Some(output);
        }
        let ceilings = self.current_budgets(cap)?;
        let currents = limit_currents(self.previous_current, ceilings);
        if !self.commands_safe(currents, cap) {
            return None;
        }
        let current_limited = currents != self.previous_current;
        // Keep the rate limiter aligned with any immediate downward cut.
        let held_caps = std::array::from_fn(|i| {
            if currents[i] == self.previous_current[i] {
                self.previous_targets[i]
            } else {
                Watts::new(
                    power(currents[i], self.config.pi[i].resistance_min.get())
                        .min(self.previous_targets[i].get()),
                )
                .expect("validated power bound")
            }
        });
        let previous_target = self.previous_target();
        let held = self
            .allocator
            .step(
                previous_target,
                held_caps,
                Watts::new(cap)?,
                [true; 2],
                tick.dt,
            )
            .ok()?;
        self.previous_current = currents;
        self.previous_targets = held.targets;
        self.stale_held = true;
        Some(ControlOutput {
            requested,
            limited: held.limited,
            measured: None,
            targets: held.targets,
            action: Actuation::Drive(currents),
            equivalence: if current_limited || held.hard_limit {
                Equivalence::Limited
            } else {
                Equivalence::Settling
            },
            fault: None,
            warning: Some(Warning::MeasurementStale),
            slew_limited: held.slew_limited,
            power_limited: held.hard_limit,
            current_limited,
        })
    }

    fn drive(
        &mut self,
        tick: Tick,
        cap: f64,
        samples: [TimedPowerSample; 2],
        measured: Option<Watts>,
    ) -> Result<ControlOutput, Fault> {
        let (mode, request) = self.request(tick, cap)?;
        if request.get() < 0.0 {
            let mut out = self.off(Some(request), measured, Equivalence::Limited);
            out.power_limited = true;
            return Ok(out);
        }
        let ceilings = self.current_budgets(cap).ok_or(Fault::Calculation)?;
        let capacities = [0, 1].map(|i| {
            Watts::new(power(ceilings[i], self.config.pi[i].resistance_min.get())).unwrap()
        });
        let transfer = self.mode.is_some_and(|previous| previous != mode);
        let reference = if transfer {
            self.previous_target()
        } else {
            request
        };
        let allocation = self
            .allocator
            .step(
                reference,
                capacities,
                Watts::new(cap).unwrap(),
                [true; 2],
                tick.dt,
            )
            .map_err(|_| Fault::Calculation)?;
        let power_limited = allocation.hard_limit
            || request.get() > capacities.iter().map(|value| value.get()).sum::<f64>();
        let mut currents = [zero_current(); 2];
        let mut current_limited = false;
        for i in 0..2 {
            let target = allocation.targets[i];
            let sample = Some(samples[i].sample);
            if transfer {
                // Preserve the previous command on the transfer tick except
                // where a tighter hard envelope requires an immediate cut.
                currents[i] = if self.previous_current[i] <= ceilings[i] {
                    self.previous_current[i]
                } else {
                    ceilings[i]
                };
                self.pi[i]
                    .rebase(target, sample, currents[i])
                    .map_err(|_| Fault::Calculation)?;
                current_limited |= currents[i] < self.previous_current[i];
            } else {
                let max_current = if target.get() == 0.0 {
                    zero_current()
                } else {
                    ceilings[i]
                };
                let out = self.pi[i]
                    .step(target, sample, tick.dt, max_current)
                    .map_err(|_| Fault::Calculation)?;
                currents[i] = out.current;
                current_limited |= out.saturated;
            }
            self.last_sample[i] = Some(samples[i].at);
        }
        let bounded = limit_currents(currents, ceilings);
        for (i, current) in bounded.iter().copied().enumerate() {
            if current != currents[i] {
                current_limited = true;
                self.pi[i]
                    .rebase(allocation.targets[i], Some(samples[i].sample), current)
                    .map_err(|_| Fault::Calculation)?;
            }
        }
        let currents = bounded;
        if !self.commands_safe(currents, cap) {
            return Err(Fault::Calculation);
        }
        self.mode = Some(mode);
        self.previous_current = currents;
        self.previous_targets = allocation.targets;
        let measured = measured.ok_or(Fault::MeasurementUnavailable)?;
        let tolerance = (0.01 * request.get()).max(0.005);
        let tracking = (measured.get() - request.get()).abs() <= tolerance;
        let equivalence = if power_limited || (current_limited && !tracking) {
            Equivalence::Limited
        } else if transfer || allocation.slew_limited || !tracking {
            Equivalence::Settling
        } else {
            Equivalence::WithinTolerance
        };
        Ok(ControlOutput {
            requested: Some(request),
            limited: allocation.limited,
            measured: Some(measured),
            targets: allocation.targets,
            action: Actuation::Drive(currents),
            equivalence,
            fault: None,
            warning: None,
            slew_limited: transfer || allocation.slew_limited,
            power_limited,
            current_limited,
        })
    }

    fn request(&mut self, tick: Tick, cap: f64) -> Result<(ActiveMode, Watts), Fault> {
        match tick.demand {
            Demand::Power(p) => Ok((ActiveMode::Power(p), p)),
            Demand::PowerProfile(p) => Ok((ActiveMode::Profile, p)),
            Demand::VirtualBody(p) => Ok((ActiveMode::Virtual, p)),
            Demand::TemperatureHold {
                setpoint, stage2, ..
            } => {
                let cfg = self
                    .config
                    .temperature_hold
                    .ok_or(Fault::TemperatureInterlock)?;
                let error = setpoint.get() - stage2.get();
                let proportional = cfg.kp.get() * error;
                let increment = cfg.ki.get() * error * tick.dt.get();
                if !proportional.is_finite() || !increment.is_finite() {
                    return Err(Fault::Calculation);
                }
                let mode = ActiveMode::Temperature(setpoint);
                if self.mode != Some(mode) {
                    self.temperature_integral =
                        (self.previous_target().get() - proportional).clamp(-cap, cap);
                } else {
                    let candidate = (self.temperature_integral + increment).clamp(-cap, cap);
                    let proposed = proportional + candidate;
                    if (0.0..=cap).contains(&proposed)
                        || (proposed > cap && increment < 0.0)
                        || (proposed < 0.0 && increment > 0.0)
                    {
                        self.temperature_integral = candidate;
                    }
                }
                let request = Watts::new(proportional + self.temperature_integral)
                    .ok_or(Fault::Calculation)?;
                Ok((mode, request))
            }
            Demand::Observe | Demand::Replay => unreachable!("inactive modes handled in tick"),
        }
    }

    fn is_fresh(&self, sample: Seconds, now: Seconds) -> bool {
        sample.get() >= 0.0
            && sample <= now
            && now.get() - sample.get() <= self.config.max_sample_age.get() + 1e-9
    }

    fn previous_target(&self) -> Watts {
        Watts::new(self.previous_targets[0].get() + self.previous_targets[1].get())
            .expect("validated target sum")
    }

    fn total_measured(samples: [Option<TimedPowerSample>; 2]) -> Option<Watts> {
        let [Some(a), Some(b)] = samples else {
            return None;
        };
        let watts = |s: PowerSample| {
            (s.current.get() >= 0.0 && s.voltage.get() >= 0.0)
                .then_some(s.current.get() * s.voltage.get())
        };
        Watts::new(watts(a.sample)? + watts(b.sample)?)
    }

    fn current_budgets(&self, cap: f64) -> Option<[Amperes; 2]> {
        let total = [0, 1]
            .into_iter()
            .map(|i| {
                upper_power(
                    self.source_currents[i],
                    self.config.pi[i].resistance_max.get(),
                )
            })
            .sum::<f64>();
        let mut factor = if total > cap {
            (cap / total).sqrt()
        } else {
            1.0
        };
        for _ in 0..8 {
            let mut ceilings = [zero_current(); 2];
            for (i, ceiling) in ceilings.iter_mut().enumerate() {
                *ceiling = quantized_ceiling(
                    self.source_currents[i].get() * factor,
                    self.config.pi[i].current_quantum,
                )?;
            }
            if self.commands_safe(ceilings, cap) {
                return Some(ceilings);
            }
            // Move inward if rounding placed a computed boundary above a hard cap.
            factor = factor.next_down().max(0.0);
        }
        None
    }

    fn commands_safe(&self, currents: [Amperes; 2], cap: f64) -> bool {
        let mut sum = 0.0;
        for (i, current) in currents.into_iter().enumerate() {
            let branch = self.config.branches[i];
            let (_, max) = branch.resistance.bounds();
            // Outward rounding protects the bound from different associations of I²R.
            let watts = upper_power(current, max.get());
            if watts > branch.heater_limit.unwrap().get()
                || !check_current(max, branch.loop_resistance, current, self.config.margins)
                    .unwrap_or(false)
            {
                return false;
            }
            sum += watts;
        }
        sum == 0.0 || sum.next_up() <= cap
    }

    fn clear_output_history(&mut self) {
        self.pi.iter_mut().for_each(Pi::reset);
        self.allocator.reset();
        self.mode = None;
        self.previous_current = [zero_current(); 2];
        self.previous_targets = [zero_watts(); 2];
        self.temperature_integral = 0.0;
        self.stale_held = false;
    }

    fn off(
        &mut self,
        requested: Option<Watts>,
        measured: Option<Watts>,
        equivalence: Equivalence,
    ) -> ControlOutput {
        self.clear_output_history();
        ControlOutput {
            requested,
            limited: zero_watts(),
            measured,
            targets: [zero_watts(); 2],
            action: Actuation::Off,
            equivalence,
            fault: self.fault,
            warning: None,
            slew_limited: false,
            power_limited: false,
            current_limited: false,
        }
    }

    fn latch(
        &mut self,
        fault: Fault,
        requested: Option<Watts>,
        measured: Option<Watts>,
    ) -> ControlOutput {
        self.fault.get_or_insert(fault);
        self.off(requested, measured, Equivalence::Fault)
    }
}

fn limit_currents(currents: [Amperes; 2], ceilings: [Amperes; 2]) -> [Amperes; 2] {
    std::array::from_fn(|i| {
        if currents[i] <= ceilings[i] {
            currents[i]
        } else {
            ceilings[i]
        }
    })
}

fn quantized_ceiling(current: f64, step: Option<Amperes>) -> Option<Amperes> {
    let Some(step) = step else {
        return Amperes::new(current);
    };
    let step = step.get();
    let n = (current / step).floor();
    let snapped = n * step;
    Amperes::new(if snapped > current {
        (n - 1.0) * step
    } else {
        snapped
    })
}
