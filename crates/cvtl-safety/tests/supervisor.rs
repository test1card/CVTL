use std::time::Duration;

use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    ControlConfig, Controller, Demand, HeatPermit, Tick, TimedPowerSample,
};
use cvtl_core::units::{Amperes, Kelvin, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_safety::{
    machine::{MachineState, OffEvidence},
    monitor::{BranchReading, CoolingPhase, MonitorInput, ProtectionProfile, Temperatures},
    supervisor::{Effect, JournalError, OperatorAction, SafetyJournal, SafetyRecord, Supervisor},
    Branch, FaultReason,
};

fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn k(x: f64) -> Kelvin {
    Kelvin::new(x).unwrap()
}

fn frame(step: u64) -> MonitorInput {
    let monotonic = Duration::from_millis(1000 + 100 * step);
    MonitorInput {
        monotonic,
        wall_time_s: Seconds::new(1000.0 + monotonic.as_secs_f64()).unwrap(),
        branches: [Some(BranchReading {
            current_a: a(0.0),
            voltage_v: Volts::new(0.0).unwrap(),
            command_power_w: w(0.0),
        }); 2],
        branch_channels: Branch::ALL,
        temperatures: Some(Temperatures {
            stage1_k: k(50.0),
            stage2_k: k(40.0),
            measured_at: monotonic,
            stage1_channel: 1,
            stage2_channel: 2,
        }),
        source_connected: true,
        thermometer_connected: true,
        missed_tick: false,
        expect_tick: false,
        queue_fault: false,
        model_valid: true,
        journal_ok: true,
        cooling_phase: CoolingPhase::Cooling,
        allow_stale_once: true,
    }
}

#[derive(Default)]
struct Journal {
    fail: bool,
    warnings: usize,
}
impl SafetyJournal for Journal {
    fn record(&mut self, event: SafetyRecord<'_>) -> Result<(), JournalError> {
        if self.fail {
            return Err(JournalError);
        }
        if matches!(
            event,
            SafetyRecord::Control {
                warning: Some(_),
                ..
            }
        ) {
            self.warnings += 1;
        }
        Ok(())
    }
}

fn off_verified() -> Supervisor {
    off_verified_with_profile(ProtectionProfile::test_only_example())
}

fn off_verified_with_profile(profile: ProtectionProfile) -> Supervisor {
    let mut s = Supervisor::new(profile).unwrap();
    s.connect().unwrap();
    let Effect::DisableBoth(token) = s.request_off().unwrap() else {
        panic!("OFF")
    };
    for b in Branch::ALL {
        s.confirm_off(token, b, OffEvidence::ReportedOff).unwrap();
    }
    s
}

fn running(journal: &mut Journal) -> Supervisor {
    running_with_profile(journal, ProtectionProfile::test_only_example())
}

fn running_with_profile(journal: &mut Journal, profile: ProtectionProfile) -> Supervisor {
    let mut s = off_verified_with_profile(profile);
    s.operate(OperatorAction::Arm, &frame(0), journal).unwrap();
    let start = s
        .operate(OperatorAction::Start, &frame(1), journal)
        .unwrap();
    let Some(Effect::EnableBoth(token)) = start.effect else {
        panic!("ON")
    };
    assert!(matches!(start.permit, HeatPermit::Inhibit));
    for b in Branch::ALL {
        s.confirm_on(token, b).unwrap();
    }
    assert_eq!(s.state(), MachineState::Running);
    s
}

fn controller() -> Controller {
    let r = |v| Ohms::new(v).unwrap();
    let s = |v| Seconds::new(v).unwrap();
    let spec = ChannelSpec {
        resistance: ResistanceRange::new(r(10.), r(11.)).unwrap(),
        loop_resistance: r(0.),
        heater_limit: Some(w(4.)),
    };
    let pi = PiConfig {
        kp: 0.3,
        ki: PerSecond::new(1.).unwrap(),
        integral_limit: w(2.),
        resistance_initial: r(10.),
        resistance_min: r(10.),
        resistance_max: r(11.),
        resistance_tau: s(0.3),
        min_current: a(0.001),
        current_quantum: None,
        derivative: None,
    };
    let config = ControlConfig {
        pi: [pi; 2],
        branches: [spec; 2],
        margins: Margins::new(0.9, 0.9).unwrap(),
        total_limit: w(6.),
        max_sample_age: s(0.05),
        temperature_hold: None,
    };
    let rate = WattsPerSecond::new(10.).unwrap();
    Controller::new(
        config,
        Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap(),
    )
    .unwrap()
}

fn control_tick(input: &MonitorInput, sample_at: Duration) -> Tick {
    Tick {
        demand: Demand::Power(w(1.0)),
        permit: HeatPermit::Inhibit,
        branches_available: [true; 2],
        measurements: [Some(TimedPowerSample {
            at: Seconds::new(sample_at.as_secs_f64()).unwrap(),
            sample: PowerSample {
                current: a(0.0),
                voltage: Volts::new(0.0).unwrap(),
            },
        }); 2],
        now: Seconds::new(input.monotonic.as_secs_f64()).unwrap(),
        dt: Seconds::new(0.1).unwrap(),
    }
}

#[test]
fn journal_failure_before_enable_cannot_arm_output() {
    let mut s = off_verified();
    let mut journal = Journal::default();
    s.operate(OperatorAction::Arm, &frame(0), &mut journal)
        .unwrap();
    journal.fail = true;
    let out = s
        .operate(OperatorAction::Start, &frame(1), &mut journal)
        .unwrap();
    assert!(matches!(out.effect, Some(Effect::DisableBoth(_))));
    assert_eq!(s.state(), MachineState::FaultLatched);
    assert_eq!(s.fault_reason(), Some(FaultReason::JournalFailure));
}

#[test]
fn a_deliberate_pause_between_operator_actions_is_not_a_missed_tick() {
    let mut s = off_verified();
    let mut journal = Journal::default();
    s.operate(OperatorAction::Arm, &frame(0), &mut journal)
        .unwrap();
    let start = s
        .operate(OperatorAction::Start, &frame(100), &mut journal)
        .unwrap();
    assert!(matches!(start.effect, Some(Effect::EnableBoth(_))));
    assert_eq!(s.fault_reason(), None);
}

#[test]
fn partial_enable_requests_off_on_both_branches_and_invalidates_on() {
    let mut s = off_verified();
    let mut journal = Journal::default();
    s.operate(OperatorAction::Arm, &frame(0), &mut journal)
        .unwrap();
    let start = s
        .operate(OperatorAction::Start, &frame(1), &mut journal)
        .unwrap();
    let Some(Effect::EnableBoth(on)) = start.effect else {
        panic!("ON")
    };
    s.confirm_on(on, Branch::A).unwrap();
    let off = s.enable_unconfirmed(on).unwrap();
    assert!(matches!(off, Effect::DisableBoth(_)));
    assert!(!s.can_enable(on));
    assert!(s.confirm_on(on, Branch::B).is_err());
    assert_eq!(s.fault_reason(), Some(FaultReason::OutputUnconfirmed));
    assert_eq!(s.output_state(), cvtl_safety::machine::OutputState::Unknown);
}

#[test]
fn warning_is_logged_and_a_tick_can_be_used_for_actuation_only_once() {
    let mut journal = Journal::default();
    let mut s = running(&mut journal);
    let input = frame(2);
    assert!(matches!(
        s.observe(&input, &mut journal).permit,
        HeatPermit::Limit(_)
    ));
    let mut c = controller();
    let command = s.regulate(
        input.monotonic,
        &mut c,
        control_tick(&input, input.monotonic),
        &mut journal,
    );
    assert!(command
        .currents
        .is_some_and(|currents| currents[0] > a(0.0)));
    assert!(command.effect.is_none());

    let stale = frame(3);
    s.observe(&stale, &mut journal);
    let held = s.regulate(
        stale.monotonic,
        &mut c,
        control_tick(&stale, input.monotonic),
        &mut journal,
    );
    assert_eq!(held.currents, command.currents);
    assert_eq!(journal.warnings, 1);
    assert_eq!(s.fault_reason(), None);
    let duplicate = s.regulate(
        stale.monotonic,
        &mut c,
        control_tick(&stale, input.monotonic),
        &mut journal,
    );
    assert!(duplicate.currents.is_none());
    assert!(matches!(duplicate.effect, Some(Effect::DisableBoth(_))));
    assert_eq!(s.fault_reason(), Some(FaultReason::ControllerFailure));
}

#[test]
fn journal_failure_after_control_blocks_commands_and_requests_off() {
    let mut journal = Journal::default();
    let mut s = running(&mut journal);
    let input = frame(2);
    assert!(matches!(
        s.observe(&input, &mut journal).permit,
        HeatPermit::Limit(_)
    ));
    journal.fail = true;
    let command = s.regulate(
        input.monotonic,
        &mut controller(),
        control_tick(&input, input.monotonic),
        &mut journal,
    );
    assert!(command.currents.is_none());
    assert!(matches!(command.effect, Some(Effect::DisableBoth(_))));
    assert_eq!(s.fault_reason(), Some(FaultReason::JournalFailure));
}

#[test]
fn inconsistent_controller_limits_cannot_release_a_drive_command() {
    let mut journal = Journal::default();
    let mut profile = ProtectionProfile::test_only_example();
    profile.max_current_a = [a(0.01); 2];
    let mut s = running_with_profile(&mut journal, profile);
    let input = frame(2);
    s.observe(&input, &mut journal);
    let output = s.regulate(
        input.monotonic,
        &mut controller(),
        control_tick(&input, input.monotonic),
        &mut journal,
    );
    assert!(output.currents.is_none());
    assert!(matches!(output.effect, Some(Effect::DisableBoth(_))));
    assert_eq!(
        s.fault_reason(),
        Some(FaultReason::BranchCurrent(Branch::A))
    );
}
