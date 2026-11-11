// FILE: crates/cvtl-cli/src/replay/modes.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Verify the recorded operator mode before controller replay
//   SCOPE: Accepted Start timing, demand, and continuity of measured-energy intervals
//   DEPENDS: M-CVTL-SIM, M-CVTL-STORE, M-CVTL-ANALYSIS
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   ModesReplay - state for the selected operator profile
// END_MODULE_MAP

use std::time::Duration;

use cvtl_analysis::m8::Model;
use cvtl_sim::modes::{
    compensation_emulation, Compensation, HeldPort, ModeSetup, ModeSnapshot, VirtualBody,
};
use cvtl_store::record::{
    Demand, ModeBodyChangeV1, ModeChangeOutcomeV1, ModeCompensationV1, ModeStepV1,
};

use super::{a1::within_thermal_roundoff, Diagnostic, Frame, Operation};

pub(super) struct ModeInput {
    pub demand: Demand,
    pub step: Option<ModeStepV1>,
}

pub(super) struct ModesReplay {
    snapshot: ModeSnapshot,
    body: Option<VirtualBody>,
    compensation: Option<Compensation>,
    start_ns: Option<u64>,
    first_start_ns: Option<u64>,
    last_interval_end_ns: Option<u64>,
    body_failed: bool,
}

impl ModesReplay {
    pub(super) fn new(
        snapshot: ModeSnapshot,
        body: Option<VirtualBody>,
        compensation: Option<Compensation>,
    ) -> Self {
        Self {
            snapshot,
            body,
            compensation,
            start_ns: None,
            first_start_ns: None,
            last_interval_end_ns: None,
            body_failed: false,
        }
    }

    pub(super) fn mode(&self) -> &ModeSetup {
        &self.snapshot.mode
    }

    pub(super) fn start(&mut self, frame: &Frame, op: &Operation) -> Result<(), Diagnostic> {
        let measured_ns = frame.measured_ns();
        if let Some(body) = &mut self.body {
            if self.body_failed {
                return Err(op.mismatch(frame.seq, "Start после отказа модели тела"));
            }
            let current = sample(body, frame, self.snapshot.mode.temperature_age())
                .ok_or_else(|| op.insufficient("Нет пригодного измерения порта при Start"))?;
            if self.first_start_ns.is_none() {
                body.observe(current);
            }
        }
        self.first_start_ns.get_or_insert(measured_ns);
        self.start_ns = Some(measured_ns);
        Ok(())
    }

    pub(super) fn off(&mut self) {
        self.start_ns = None;
        self.clear_compensation();
    }

    pub(super) fn clear_compensation(&mut self) {
        if let Some(compensation) = &mut self.compensation {
            compensation.clear();
        }
    }

    pub(super) fn has_compensation(&self) -> bool {
        self.compensation.is_some()
    }

    pub(super) fn request_pair(&mut self, frame: &Frame) -> Option<(bool, bool)> {
        let max_age = self.snapshot.mode.temperature_age();
        let compensation = self.compensation.as_mut()?;
        let prior_ready = frame.scheduled_ns <= frame.measured_ns()
            && compensation.ready(frame.scheduled_ns, max_age);
        let current = fixture_sample(frame, max_age, Some(frame.scheduled_ns));
        let current_ready = matches!(
            compensation.evaluate(current, frame.measured_ns(), max_age),
            ModeCompensationV1::Ready { .. }
        );
        Some((prior_ready, current_ready))
    }

    pub(super) fn verify_tick(
        &mut self,
        frame: &Frame,
        op: &Operation,
        running: bool,
    ) -> Result<Option<u64>, Diagnostic> {
        let input = frame.mode.as_ref().expect("operator profile input");
        let thermal = frame.thermal.expect("operator profile temperatures");
        let start = if running {
            Some(
                self.start_ns
                    .ok_or_else(|| op.insufficient("Отсутствует подтверждённый Start"))?,
            )
        } else {
            None
        };
        let step = self.verify_interval(frame, op, input.step.as_ref())?;
        let max_age = self.snapshot.mode.temperature_age();
        let fixture = self.compensation.as_mut().map(|compensation| {
            let current = fixture_sample(frame, max_age, step.map(|step| step.interval_end_ns));
            compensation.evaluate(current, frame.measured_ns(), max_age)
        });
        let expected = if let Some(body) = &mut self.body {
            if self.body_failed && (frame.monitor.model_valid || step.is_some()) {
                return Err(op.mismatch(frame.seq, "Расчёт возобновлён после отказа модели тела"));
            }
            match step {
                Some(step) => {
                    let request = verify_body(body, step, frame, op, max_age, fixture)?;
                    self.body_failed = !frame.monitor.model_valid;
                    match (start, request) {
                        (Some(_), Some(requested_w)) => Demand::VirtualBody { requested_w },
                        _ => Demand::Observe,
                    }
                }
                None => {
                    self.body_failed |= self.first_start_ns.is_some() && !frame.monitor.model_valid;
                    Demand::Observe
                }
            }
        } else if frame.monitor.model_valid {
            self.mode()
                .ordinary_demand(start, frame.measured_ns(), &thermal.temperatures)
                .map_err(|_| op.mismatch(frame.seq, "Не удалось пересчитать запрос режима"))?
        } else {
            Demand::Observe
        };
        if !demand_matches(expected, input.demand) {
            return Err(op.mismatch(frame.seq, "Запрос режима отличается от пересчитанного"));
        }
        Ok(step.map(|step| step.interval_end_ns))
    }

    fn verify_interval<'a>(
        &self,
        frame: &Frame,
        op: &Operation,
        step: Option<&'a ModeStepV1>,
    ) -> Result<Option<&'a ModeStepV1>, Diagnostic> {
        let Some(step) = step else {
            if self.first_start_ns.is_some() && frame.monitor.model_valid {
                return Err(op.insufficient("Отсутствует тепловой интервал режима"));
            }
            return Ok(None);
        };
        let start = self
            .first_start_ns
            .ok_or_else(|| op.mismatch(frame.seq, "Тепловой интервал до Start"))?;
        let end = step.interval_end_ns;
        let begin = end
            .checked_sub(step.interval_ns)
            .ok_or_else(|| op.mismatch(frame.seq, "Неверная длительность теплового интервала"))?;
        if step.interval_ns == 0
            || begin < start
            || end > frame.measured_ns()
            || self
                .last_interval_end_ns
                .is_some_and(|previous| begin != previous)
        {
            return Err(op.mismatch(frame.seq, "Нарушена граница теплового интервала режима"));
        }
        if self.body.is_some() != step.body.is_some() {
            return Err(op.mismatch(frame.seq, "Тепловой шаг тела вне выбранного режима"));
        }
        Ok(Some(step))
    }

    pub(super) fn accept_frame(
        &mut self,
        frame: &Frame,
        interval_end_ns: Option<u64>,
        accept_fixture: bool,
    ) {
        let max_age = self.snapshot.mode.temperature_age();
        if let Some(end_ns) = interval_end_ns {
            self.last_interval_end_ns = Some(end_ns);
            if let Some(body) = self.body.as_mut().filter(|_| frame.monitor.model_valid) {
                if let Some(current) = current_port(body, frame, max_age, end_ns) {
                    body.observe(current);
                }
            }
        }
        if let Some(compensation) = self.compensation.as_mut().filter(|_| accept_fixture) {
            compensation.accept(fixture_sample(frame, max_age, interval_end_ns));
        }
    }

    pub(super) fn analysis_model(&self) -> Result<Model, String> {
        if self.has_compensation() {
            return Err(
                "Холодопроизводительность не определяется при переменной теплоёмкости насадки"
                    .into(),
            );
        }
        if matches!(self.mode(), ModeSetup::Observe) {
            return Err("Наблюдение не содержит тепловых интервалов".into());
        }
        let head = &self.snapshot.head;
        Ok(Model {
            capacity_stage2_j_per_k: head.capacity_j_per_k[1],
            capacity_tf_j_per_k: head.capacity_j_per_k[2],
            stage_link_w_per_k: head.links_w_per_k[0],
            fixture_link_w_per_k: head.links_w_per_k[1],
            initial_k: head.initial_k,
            other_w: Some(0.0),
            wire: None,
            group: None,
        })
    }
}

fn sample(body: &VirtualBody, frame: &Frame, max_age: Duration) -> Option<HeldPort> {
    body.sample(&frame.thermal?.temperatures, frame.measured_ns(), max_age)
}

fn current_port(
    body: &VirtualBody,
    frame: &Frame,
    max_age: Duration,
    end_ns: u64,
) -> Option<HeldPort> {
    sample(body, frame, max_age).filter(|current| current.read_started_ns() >= end_ns)
}

fn fixture_sample(
    frame: &Frame,
    max_age: Duration,
    interval_end_ns: Option<u64>,
) -> Option<HeldPort> {
    Compensation::sample(&frame.thermal?.temperatures, frame.measured_ns(), max_age)
        .filter(|current| interval_end_ns.is_none_or(|end| current.read_started_ns() >= end))
}

fn verify_body(
    body: &mut VirtualBody,
    step: &ModeStepV1,
    frame: &Frame,
    op: &Operation,
    max_age: Duration,
    fixture: Option<ModeCompensationV1>,
) -> Result<Option<f64>, Diagnostic> {
    let recorded = step.body.as_ref().expect("profile checked before replay");
    if !compensation_matches(fixture, recorded.compensation) {
        return Err(op.mismatch(frame.seq, "Условия компенсации отличаются от расчёта"));
    }
    let edit = recorded
        .change
        .as_ref()
        .map(|change| {
            let edit = serde_json::from_value(change.request.clone()).map_err(|_| {
                op.mismatch(frame.seq, "Некорректная запись запроса изменения тела")
            })?;
            body.validate_edit(&edit)
                .map_err(|_| op.mismatch(frame.seq, "Запрос изменения тела непригоден до шага"))?;
            Ok(edit)
        })
        .transpose()?;
    let mut computed = body
        .advance(step.interval_ns, step.interval_end_ns, max_age)
        .map_err(|_| op.mismatch(frame.seq, "Не удалось пересчитать тепловой шаг тела"))?;
    if computed.port != recorded.port
        || computed.held_k.to_bits() != recorded.held_k.to_bits()
        || computed.held_read_started_ns != recorded.held_read_started_ns
        || computed.held_age_ns != recorded.held_age_ns
    {
        return Err(op.mismatch(
            frame.seq,
            "Удержанное наблюдение порта отличается от входов",
        ));
    }
    let current = current_port(body, frame, max_age, step.interval_end_ns);
    let completion = body.complete_tick(
        &mut computed,
        current,
        edit,
        compensation_emulation(fixture),
    );
    if completion.is_ok() != frame.monitor.model_valid {
        return Err(op.mismatch(frame.seq, "Состояние модели тела отличается от расчёта"));
    }
    let (dh_scale, request) = completion.unwrap_or((1.0, None));
    if !within_thermal_roundoff(computed.temperature_k, recorded.temperature_k, 1.0)
        || !optional_roundoff(computed.q_v_w, recorded.q_v_w, 1.0)
        || !change_matches(computed.change.as_ref(), recorded.change.as_ref(), dh_scale)
    {
        return Err(op.mismatch(
            frame.seq,
            "Тепловой шаг или изменение тела отличается от расчёта",
        ));
    }
    Ok(request)
}

fn compensation_matches(
    computed: Option<ModeCompensationV1>,
    recorded: Option<ModeCompensationV1>,
) -> bool {
    use ModeCompensationV1::Ready;
    match (computed, recorded) {
        (
            Some(Ready {
                previous_tf_k,
                previous_read_started_ns,
                current_tf_k,
                current_read_started_ns,
                d_tf_k_per_s,
                fixture_capacity_j_per_k,
                fixture_parasitic_w,
            }),
            Some(Ready {
                previous_tf_k: recorded_previous_tf_k,
                previous_read_started_ns: recorded_previous_ns,
                current_tf_k: recorded_current_tf_k,
                current_read_started_ns: recorded_current_ns,
                d_tf_k_per_s: recorded_rate,
                fixture_capacity_j_per_k: recorded_capacity,
                fixture_parasitic_w: recorded_parasitic,
            }),
        ) => {
            previous_tf_k.to_bits() == recorded_previous_tf_k.to_bits()
                && previous_read_started_ns == recorded_previous_ns
                && current_tf_k.to_bits() == recorded_current_tf_k.to_bits()
                && current_read_started_ns == recorded_current_ns
                && fixture_parasitic_w.to_bits() == recorded_parasitic.to_bits()
                && within_thermal_roundoff(d_tf_k_per_s, recorded_rate, 1.0)
                && within_thermal_roundoff(fixture_capacity_j_per_k, recorded_capacity, 1.0)
        }
        _ => computed == recorded,
    }
}

fn optional_roundoff(computed: Option<f64>, recorded: Option<f64>, scale: f64) -> bool {
    match (computed, recorded) {
        (Some(computed), Some(recorded)) => within_thermal_roundoff(computed, recorded, scale),
        (None, None) => true,
        _ => false,
    }
}

fn change_matches(
    computed: Option<&ModeBodyChangeV1>,
    recorded: Option<&ModeBodyChangeV1>,
    dh_scale: f64,
) -> bool {
    use ModeChangeOutcomeV1::{Applied, Rejected};
    match (computed, recorded) {
        (None, None) => true,
        (Some(computed), Some(recorded)) if computed.request == recorded.request => {
            match (computed.outcome, recorded.outcome) {
                (Rejected, Rejected) => true,
                (
                    Applied {
                        temperature_before_k: before,
                        temperature_after_k: after,
                        dh_event_j: dh,
                    },
                    Applied {
                        temperature_before_k: recorded_before,
                        temperature_after_k: recorded_after,
                        dh_event_j: recorded_dh,
                    },
                ) => {
                    within_thermal_roundoff(before, recorded_before, 1.0)
                        && within_thermal_roundoff(after, recorded_after, 1.0)
                        && optional_roundoff(dh, recorded_dh, dh_scale)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn demand_matches(expected: Demand, recorded: Demand) -> bool {
    match (expected, recorded) {
        (Demand::Observe, Demand::Observe) => true,
        (
            Demand::Power {
                requested_w: expected,
            },
            Demand::Power {
                requested_w: recorded,
            },
        )
        | (
            Demand::PowerProfile {
                requested_w: expected,
            },
            Demand::PowerProfile {
                requested_w: recorded,
            },
        ) => expected.to_bits() == recorded.to_bits(),
        (
            Demand::VirtualBody {
                requested_w: expected,
            },
            Demand::VirtualBody {
                requested_w: recorded,
            },
        ) => {
            (expected < 0.0) == (recorded < 0.0) && within_thermal_roundoff(expected, recorded, 1.0)
        }
        (
            Demand::TemperatureHold {
                setpoint_k,
                stage1_k,
                stage2_k,
                at_ns,
            },
            Demand::TemperatureHold {
                setpoint_k: recorded_setpoint,
                stage1_k: recorded_stage1,
                stage2_k: recorded_stage2,
                at_ns: recorded_at,
            },
        ) => {
            setpoint_k.to_bits() == recorded_setpoint.to_bits()
                && stage1_k.to_bits() == recorded_stage1.to_bits()
                && stage2_k.to_bits() == recorded_stage2.to_bits()
                && at_ns == recorded_at
        }
        _ => false,
    }
}
