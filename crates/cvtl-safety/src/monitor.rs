use cvtl_core::units::{Amperes, Kelvin, KelvinPerSecond, Seconds, Volts, Watts};
// Deterministic, simulator-only protection thresholds and observation logic.
// These numbers are illustrative and do not authorize real equipment.
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ProtectionProfile {
    pub max_current_a: [Amperes; 2],
    pub max_voltage_v: [Volts; 2],
    pub max_branch_power_w: [Watts; 2],
    pub max_total_power_w: Watts,
    pub max_temperature_age: Duration,
    pub max_tick_gap: Duration,
    pub phase_power_limits_w: [Watts; 3],
    pub tracking_tolerance_w: Watts,
    pub tracking_duration: Duration,
    pub wall_jump_tolerance_s: Seconds,
    pub stage1_channel: u8,
    pub stage2_channel: u8,
    pub head: HeadProfile,
}
#[derive(Clone, Debug)]
pub struct HeadProfile {
    pub inversion_warning_k: Kelvin,
    pub inversion_derate_k: Kelvin,
    pub inversion_trip_k: Kelvin,
    pub temperature_warning_k: Kelvin,
    pub temperature_derate_k: Kelvin,
    pub temperature_trip_k: Kelvin,
    pub min_temperature_k: Kelvin,
    pub derated_power_w: Watts,
    pub max_rate_k_per_s: KelvinPerSecond,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileError;
impl ProtectionProfile {
    /// Explicitly synthetic values; never a default or an equipment specification.
    pub fn test_only_example() -> Self {
        Self {
            max_current_a: [Amperes::new(10.0).unwrap(); 2],
            max_voltage_v: [Volts::new(10.0).unwrap(); 2],
            max_branch_power_w: [Watts::new(20.0).unwrap(); 2],
            max_total_power_w: Watts::new(30.0).unwrap(),
            max_temperature_age: Duration::from_millis(1500),
            max_tick_gap: Duration::from_millis(1500),
            phase_power_limits_w: [Watts::new(30.0).unwrap(); 3],
            tracking_tolerance_w: Watts::new(1.0).unwrap(),
            tracking_duration: Duration::from_secs(2),
            wall_jump_tolerance_s: Seconds::new(2.0).unwrap(),
            stage1_channel: 1,
            stage2_channel: 2,
            head: HeadProfile {
                inversion_warning_k: Kelvin::new(-1.0).unwrap(),
                inversion_derate_k: Kelvin::new(0.0).unwrap(),
                inversion_trip_k: Kelvin::new(1.0).unwrap(),
                temperature_warning_k: Kelvin::new(290.0).unwrap(),
                temperature_derate_k: Kelvin::new(300.0).unwrap(),
                temperature_trip_k: Kelvin::new(310.0).unwrap(),
                min_temperature_k: Kelvin::new(1.0).unwrap(),
                derated_power_w: Watts::new(1.0).unwrap(),
                max_rate_k_per_s: KelvinPerSecond::new(100.0).unwrap(),
            },
        }
    }
    pub fn validate(self) -> Result<Self, ProfileError> {
        let h = &self.head;
        let positives = self
            .max_current_a
            .into_iter()
            .map(|v| v.get())
            .chain(self.max_voltage_v.into_iter().map(|v| v.get()))
            .chain(self.max_branch_power_w.into_iter().map(|v| v.get()))
            .chain([
                self.max_total_power_w.get(),
                self.tracking_tolerance_w.get(),
                self.wall_jump_tolerance_s.get(),
                h.temperature_warning_k.get(),
                h.temperature_derate_k.get(),
                h.temperature_trip_k.get(),
                h.min_temperature_k.get(),
                h.derated_power_w.get(),
                h.max_rate_k_per_s.get(),
            ]);
        if positives.into_iter().any(|x| !x.is_finite() || x <= 0.0)
            || ![
                h.inversion_warning_k.get(),
                h.inversion_derate_k.get(),
                h.inversion_trip_k.get(),
            ]
            .iter()
            .all(|x| x.is_finite())
            || h.inversion_warning_k.get() >= h.inversion_derate_k.get()
            || h.inversion_derate_k.get() >= h.inversion_trip_k.get()
            || h.min_temperature_k.get() >= h.temperature_warning_k.get()
            || h.temperature_warning_k.get() >= h.temperature_derate_k.get()
            || h.temperature_derate_k.get() >= h.temperature_trip_k.get()
            || h.derated_power_w.get() > self.max_total_power_w.get()
            || self
                .phase_power_limits_w
                .iter()
                .any(|v| v.get() <= 0.0 || v.get() > self.max_total_power_w.get())
            || self.max_tick_gap.is_zero()
            || self.max_temperature_age.is_zero()
            || self.tracking_duration.is_zero()
            || self.stage1_channel == self.stage2_channel
        {
            return Err(ProfileError);
        }
        Ok(self)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct BranchReading {
    pub current_a: Amperes,
    pub voltage_v: Volts,
    pub command_power_w: Watts,
}
#[derive(Clone, Copy, Debug)]
pub struct Temperatures {
    pub stage1_k: Kelvin,
    pub stage2_k: Kelvin,
    pub measured_at: Duration,
    pub stage1_channel: u8,
    pub stage2_channel: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoolingPhase {
    Cooling,
    Stable,
    Warming,
}
#[derive(Clone, Debug)]
pub struct MonitorInput {
    pub monotonic: Duration,
    pub wall_time_s: Seconds,
    pub branches: [Option<BranchReading>; 2],
    pub branch_channels: [Branch; 2],
    pub temperatures: Option<Temperatures>,
    pub allow_stale_once: bool,
    pub source_connected: bool,
    pub thermometer_connected: bool,
    pub missed_tick: bool,
    pub expect_tick: bool,
    pub queue_fault: bool,
    pub model_valid: bool,
    pub journal_ok: bool,
    pub cooling_phase: CoolingPhase,
}
use crate::{Branch, FaultReason};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Warning {
    HeadApproaching,
    MeasurementStale,
}
#[derive(Clone, Debug, Default)]
pub struct MonitorOutcome {
    pub faults: Vec<FaultReason>,
    pub warnings: Vec<Warning>,
    pub derate_power_w: Option<Watts>,
}
#[derive(Debug)]
pub struct SafetyMonitor {
    profile: ProtectionProfile,
    previous_clock: Option<(Duration, f64)>,
    previous_temp: Option<(Duration, f64, f64)>,
    tracking_since: [Option<Duration>; 2],
    stale_once: bool,
    had_fresh: bool,
}
impl SafetyMonitor {
    pub fn new(profile: ProtectionProfile) -> Result<Self, ProfileError> {
        Ok(Self {
            profile: profile.validate()?,
            previous_clock: None,
            previous_temp: None,
            tracking_since: [None; 2],
            stale_once: false,
            had_fresh: false,
        })
    }
    pub fn total_limit(&self) -> Watts {
        self.profile.max_total_power_w
    }
    pub fn hard_limit(&self, phase: CoolingPhase) -> Watts {
        self.profile.phase_power_limits_w[match phase {
            CoolingPhase::Cooling => 0,
            CoolingPhase::Stable => 1,
            CoolingPhase::Warming => 2,
        }]
    }
    pub(crate) fn command_violation(
        &self,
        bounds: [cvtl_control::CommandBound; 2],
        hard_limit: Watts,
    ) -> Option<FaultReason> {
        for (branch, bound) in Branch::ALL.into_iter().zip(bounds) {
            let i = branch.index();
            if bound.current > self.profile.max_current_a[i] {
                return Some(FaultReason::BranchCurrent(branch));
            }
            if bound.sense_voltage > self.profile.max_voltage_v[i] {
                return Some(FaultReason::BranchVoltage(branch));
            }
            if bound.heater_power > self.profile.max_branch_power_w[i] {
                return Some(FaultReason::BranchPower(branch));
            }
        }
        let total = bounds[0].heater_power.get() + bounds[1].heater_power.get();
        (total > 0.0 && total.next_up() > hard_limit.get()).then_some(FaultReason::TotalPower)
    }
    pub fn evaluate(&mut self, i: &MonitorInput) -> MonitorOutcome {
        let mut o = MonitorOutcome::default();
        let p = &self.profile;
        let tick_interval = self
            .previous_clock
            .and_then(|(t, _)| i.monotonic.checked_sub(t));
        if !i.source_connected {
            o.faults.push(FaultReason::SourceLinkLost);
        }
        if !i.thermometer_connected {
            o.faults.push(FaultReason::ThermometerLinkLost);
        }
        if i.missed_tick || (i.expect_tick && tick_interval.is_some_and(|dt| dt > p.max_tick_gap)) {
            o.faults.push(FaultReason::MissedTick);
        }
        if i.queue_fault {
            o.faults.push(FaultReason::InstrumentQueue);
        }
        if !i.model_valid {
            o.faults.push(FaultReason::ModelFailure);
        }
        if !i.journal_ok {
            o.faults.push(FaultReason::JournalFailure);
        }
        if !i.wall_time_s.get().is_finite() {
            o.faults.push(FaultReason::WallClockJump);
        }
        if let Some((t, w)) = self.previous_clock {
            if let Some(dt) = i.monotonic.checked_sub(t) {
                if dt.is_zero() {
                    o.faults.push(FaultReason::MonotonicClock);
                }
                if (i.wall_time_s.get() - w - dt.as_secs_f64()).abs()
                    > p.wall_jump_tolerance_s.get()
                {
                    o.faults.push(FaultReason::WallClockJump);
                }
            } else {
                o.faults.push(FaultReason::MonotonicClock);
            }
        }
        self.previous_clock = Some((i.monotonic, i.wall_time_s.get()));
        if i.branch_channels != [Branch::A, Branch::B] {
            o.faults.push(FaultReason::ChannelMismatch);
        }
        let mut total = 0.0;
        for (n, reading) in i.branches.iter().enumerate() {
            let Some(b) = reading else {
                self.tracking_since[n] = None;
                o.faults.push(FaultReason::MeasurementUnavailable);
                continue;
            };
            if !b.current_a.get().is_finite()
                || !b.voltage_v.get().is_finite()
                || !b.command_power_w.get().is_finite()
                || b.command_power_w.get() < 0.0
            {
                self.tracking_since[n] = None;
                o.faults.push(FaultReason::MeasurementUnavailable);
                continue;
            }
            let current = b.current_a.get().abs();
            let voltage = b.voltage_v.get().abs();
            let power = current * voltage;
            if !power.is_finite() {
                self.tracking_since[n] = None;
                o.faults.push(FaultReason::MeasurementUnavailable);
                continue;
            }
            if current > p.max_current_a[n].get() {
                o.faults.push(FaultReason::BranchCurrent(if n == 0 {
                    Branch::A
                } else {
                    Branch::B
                }));
            }
            if voltage > p.max_voltage_v[n].get() {
                o.faults.push(FaultReason::BranchVoltage(if n == 0 {
                    Branch::A
                } else {
                    Branch::B
                }));
            }
            if power > p.max_branch_power_w[n].get() {
                o.faults.push(FaultReason::BranchPower(if n == 0 {
                    Branch::A
                } else {
                    Branch::B
                }));
            }
            total += power;
            if (power - b.command_power_w.get()).abs() > p.tracking_tolerance_w.get() {
                let start = *self.tracking_since[n].get_or_insert(i.monotonic);
                if i.monotonic
                    .checked_sub(start)
                    .is_some_and(|d| d >= p.tracking_duration)
                {
                    o.faults.push(FaultReason::PowerMismatch(if n == 0 {
                        Branch::A
                    } else {
                        Branch::B
                    }));
                }
            } else {
                self.tracking_since[n] = None;
            }
        }
        if !total.is_finite() {
            o.faults.push(FaultReason::MeasurementUnavailable);
        }
        if total > p.max_total_power_w.get() {
            o.faults.push(FaultReason::TotalPower);
        }
        if total > self.hard_limit(i.cooling_phase).get() {
            o.faults.push(FaultReason::HeadPower);
        }
        match i.temperatures {
            None => {
                o.faults.push(FaultReason::MeasurementUnavailable);
                self.stale_once = false;
            }
            Some(t) => {
                if t.stage1_channel != p.stage1_channel || t.stage2_channel != p.stage2_channel {
                    o.faults.push(FaultReason::ChannelMismatch);
                }
                let age = i.monotonic.checked_sub(t.measured_at);
                let backwards = self
                    .previous_temp
                    .is_some_and(|(prev, _, _)| t.measured_at < prev);
                if backwards {
                    o.faults.push(FaultReason::MonotonicClock);
                }
                if age.is_none() {
                    o.faults.push(FaultReason::MonotonicClock);
                } else if age.is_some_and(|a| a > p.max_temperature_age) {
                    if i.allow_stale_once
                        && self.had_fresh
                        && !self.stale_once
                        && tick_interval.is_some_and(|dt| {
                            !dt.is_zero()
                                && age.is_some_and(|a| {
                                    a <= p
                                        .max_temperature_age
                                        .saturating_add(dt.min(p.max_tick_gap))
                                })
                        })
                    {
                        o.warnings.push(Warning::MeasurementStale);
                        self.stale_once = true;
                    } else {
                        o.faults.push(FaultReason::TemperatureStale);
                    }
                } else if !backwards {
                    self.stale_once = false;
                    self.had_fresh = true;
                }
                let h = &p.head;
                let diff = t.stage2_k.get() - t.stage1_k.get();
                let high = t.stage1_k.get().max(t.stage2_k.get());
                if !t.stage1_k.get().is_finite()
                    || !t.stage2_k.get().is_finite()
                    || t.stage1_k.get() < h.min_temperature_k.get()
                    || t.stage2_k.get() < h.min_temperature_k.get()
                    || high >= h.temperature_trip_k.get()
                {
                    o.faults.push(FaultReason::HeadAbsoluteTemperature);
                } else if diff >= h.inversion_trip_k.get() {
                    o.faults.push(FaultReason::HeadInversion);
                } else {
                    if diff >= h.inversion_warning_k.get() || high >= h.temperature_warning_k.get()
                    {
                        o.warnings.push(Warning::HeadApproaching);
                    }
                    if diff >= h.inversion_derate_k.get() || high >= h.temperature_derate_k.get() {
                        o.derate_power_w = Some(h.derated_power_w);
                    }
                }
                if !backwards && age.is_some_and(|a| a <= p.max_temperature_age) {
                    if let Some((prev, a, b)) = self.previous_temp {
                        if let Some(dt) = t.measured_at.checked_sub(prev).filter(|d| !d.is_zero()) {
                            if ((t.stage1_k.get() - a).abs() / dt.as_secs_f64())
                                .max((t.stage2_k.get() - b).abs() / dt.as_secs_f64())
                                > h.max_rate_k_per_s.get()
                            {
                                o.faults.push(FaultReason::HeadRate);
                            }
                        }
                    }
                    if t.stage1_k.get().is_finite()
                        && t.stage2_k.get().is_finite()
                        && t.stage1_k.get() >= h.min_temperature_k.get()
                        && t.stage2_k.get() >= h.min_temperature_k.get()
                    {
                        self.previous_temp =
                            Some((t.measured_at, t.stage1_k.get(), t.stage2_k.get()));
                    }
                }
            }
        }
        o
    }
}
