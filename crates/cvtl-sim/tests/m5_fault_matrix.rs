//! Synthetic fault injection through the real monitor and safety state machine.
//! Limits here are test fixtures, not validated hardware limits.
use cvtl_core::units::{Amperes, Kelvin, Seconds, Volts, Watts};
use cvtl_safety::machine::{MachineState, OffEvidence, OutputState, SafetyMachine};
use cvtl_safety::monitor::{
    BranchReading, CoolingPhase, MonitorInput, ProtectionProfile, SafetyMonitor, Temperatures,
};
use cvtl_safety::supervisor::{
    Effect, JournalError, OperatorAction, SafetyJournal, SafetyRecord, Supervisor,
};
use cvtl_safety::{Branch, FaultReason};
use std::time::Duration;

struct MemoryJournal {
    remaining: usize,
}
impl SafetyJournal for MemoryJournal {
    fn record(&mut self, _: SafetyRecord<'_>) -> Result<(), JournalError> {
        if self.remaining == 0 {
            return Err(JournalError);
        }
        self.remaining -= 1;
        Ok(())
    }
}

fn advance(input: &mut MonitorInput) {
    input.monotonic += Duration::from_secs(1);
    input.wall_time_s = Seconds::new(input.wall_time_s.get() + 1.0).unwrap();
    input.temperatures.as_mut().unwrap().measured_at = input.monotonic;
}

fn sample() -> MonitorInput {
    MonitorInput {
        monotonic: Duration::from_secs(1),
        wall_time_s: Seconds::new(1000.0).unwrap(),
        branches: [Some(BranchReading {
            current_a: Amperes::new(0.5).unwrap(),
            voltage_v: Volts::new(2.0).unwrap(),
            command_power_w: Watts::new(1.0).unwrap(),
        }); 2],
        branch_channels: [Branch::A, Branch::B],
        temperatures: Some(Temperatures {
            stage1_k: Kelvin::new(50.0).unwrap(),
            stage2_k: Kelvin::new(40.0).unwrap(),
            measured_at: Duration::from_secs(1),
            stage1_channel: 1,
            stage2_channel: 2,
        }),
        source_connected: true,
        thermometer_connected: true,
        missed_tick: false,
        expect_tick: true,
        queue_fault: false,
        model_valid: true,
        journal_ok: true,
        allow_stale_once: false,
        cooling_phase: CoolingPhase::Cooling,
    }
}

#[derive(Default)]
struct SyntheticOutputs {
    off_requests: [bool; 2],
}
impl SyntheticOutputs {
    fn apply_off(&mut self) {
        self.off_requests = [true, true];
    }
}

fn running() -> SafetyMachine {
    let mut m = SafetyMachine::new();
    m.connect().unwrap();
    let token = m.request_off().unwrap();
    for b in [Branch::A, Branch::B] {
        m.confirm_off(token, b, OffEvidence::ReportedOff).unwrap();
    }
    m.arm().unwrap();
    let token = m.start().unwrap();
    for b in [Branch::A, Branch::B] {
        m.confirm_on(token, b).unwrap();
    }
    assert_eq!(m.state(), MachineState::Running);
    m
}

#[test]
fn each_fault_triggers_both_off_and_requires_both_confirmations() {
    type Injection = fn(&mut MonitorInput);
    let cases: Vec<(FaultReason, Injection)> = vec![
        (
            FaultReason::BranchCurrent(Branch::A),
            (|i| i.branches[0].as_mut().unwrap().current_a = Amperes::new(11.0).unwrap()),
        ),
        (
            FaultReason::BranchVoltage(Branch::A),
            (|i| i.branches[0].as_mut().unwrap().voltage_v = Volts::new(11.0).unwrap()),
        ),
        (
            FaultReason::BranchPower(Branch::A),
            (|i| {
                i.branches[0].as_mut().unwrap().current_a = Amperes::new(3.0).unwrap();
                i.branches[0].as_mut().unwrap().voltage_v = Volts::new(8.0).unwrap();
            }),
        ),
        (
            FaultReason::TotalPower,
            (|i| {
                for b in &mut i.branches {
                    b.as_mut().unwrap().current_a = Amperes::new(4.0).unwrap();
                    b.as_mut().unwrap().voltage_v = Volts::new(4.0).unwrap();
                }
            }),
        ),
        (
            FaultReason::MeasurementUnavailable,
            (|i| i.branches[0] = None),
        ),
        (
            FaultReason::SourceLinkLost,
            (|i| i.source_connected = false),
        ),
        (
            FaultReason::ThermometerLinkLost,
            (|i| i.thermometer_connected = false),
        ),
        (
            FaultReason::TemperatureStale,
            (|i| {
                i.temperatures.as_mut().unwrap().measured_at = i.monotonic - Duration::from_secs(1)
            }),
        ),
        (FaultReason::MissedTick, (|i| i.missed_tick = true)),
        (FaultReason::InstrumentQueue, (|i| i.queue_fault = true)),
        (FaultReason::ModelFailure, (|i| i.model_valid = false)),
        (FaultReason::JournalFailure, (|i| i.journal_ok = false)),
        (
            FaultReason::ChannelMismatch,
            (|i| {
                let t = i.temperatures.as_mut().unwrap();
                t.stage1_channel = 2;
                t.stage2_channel = 1;
            }),
        ),
        (
            FaultReason::ChannelMismatch,
            (|i| i.branch_channels = [Branch::B, Branch::A]),
        ),
        (
            FaultReason::HeadInversion,
            (|i| i.temperatures.as_mut().unwrap().stage2_k = Kelvin::new(51.0).unwrap()),
        ),
        (
            FaultReason::HeadAbsoluteTemperature,
            (|i| i.temperatures.as_mut().unwrap().stage1_k = Kelvin::new(311.0).unwrap()),
        ),
    ];
    for (expected, inject) in cases {
        let mut profile = ProtectionProfile::test_only_example();
        if expected == FaultReason::TemperatureStale {
            profile.max_temperature_age = Duration::from_millis(500);
        }
        let mut supervisor = Supervisor::new(profile).unwrap();
        let mut journal = MemoryJournal { remaining: 8 };
        supervisor.connect().unwrap();
        let Effect::DisableBoth(off) = supervisor.request_off().unwrap() else {
            panic!("expected OFF")
        };
        for branch in [Branch::A, Branch::B] {
            supervisor
                .confirm_off(off, branch, OffEvidence::ReportedOff)
                .unwrap();
        }
        let mut input = sample();
        let arm = supervisor
            .operate(OperatorAction::Arm, &input, &mut journal)
            .unwrap();
        assert!(arm.effect.is_none());
        advance(&mut input);
        let start = supervisor
            .operate(OperatorAction::Start, &input, &mut journal)
            .unwrap();
        let Some(Effect::EnableBoth(on)) = start.effect else {
            panic!("expected ON: {expected:?}")
        };
        for branch in [Branch::A, Branch::B] {
            assert!(supervisor.can_enable(on));
            supervisor.confirm_on(on, branch).unwrap();
        }
        assert_eq!(supervisor.state(), MachineState::Running);
        advance(&mut input);
        inject(&mut input);
        let decision = supervisor.observe(&input, &mut journal);
        let Some(Effect::DisableBoth(token)) = decision.effect else {
            panic!("missing OFF for {expected:?}")
        };
        assert_eq!(supervisor.fault_reason(), Some(expected));
        let mut outputs = SyntheticOutputs::default();
        outputs.apply_off();
        assert_eq!(outputs.off_requests, [true, true]);
        assert_eq!(supervisor.state(), MachineState::FaultLatched);
        assert_eq!(supervisor.output_state(), OutputState::Unknown);
        supervisor
            .confirm_off(token, Branch::A, OffEvidence::ReportedOff)
            .unwrap();
        assert_eq!(supervisor.output_state(), OutputState::Unknown);
        supervisor
            .confirm_off(token, Branch::B, OffEvidence::MeasuredNearZero)
            .unwrap();
        assert_eq!(supervisor.output_state(), OutputState::OffVerified);
        assert_eq!(supervisor.state(), MachineState::FaultLatched);
    }
}

#[test]
fn timed_faults_use_injected_clocks_and_latch() {
    for expected in [
        FaultReason::WallClockJump,
        FaultReason::PowerMismatch(Branch::A),
        FaultReason::HeadRate,
    ] {
        let mut monitor = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
        let mut i = sample();
        if expected == FaultReason::PowerMismatch(Branch::A) {
            i.branches[0].as_mut().unwrap().command_power_w = Watts::new(4.0).unwrap();
        }
        assert!(!monitor.evaluate(&i).faults.contains(&expected));
        i.monotonic += Duration::from_secs(3);
        i.wall_time_s = Seconds::new(
            i.wall_time_s.get()
                + if expected == FaultReason::WallClockJump {
                    100.0
                } else {
                    3.0
                },
        )
        .unwrap();
        i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
        if expected == FaultReason::HeadRate {
            i.temperatures.as_mut().unwrap().stage1_k = Kelvin::new(400.0).unwrap();
        }
        let outcome = monitor.evaluate(&i);
        assert!(
            outcome.faults.contains(&expected),
            "missing {expected:?}: {outcome:?}"
        );
        let mut machine = running();
        let token = machine.latch_fault(expected);
        let mut outputs = SyntheticOutputs::default();
        outputs.apply_off();
        assert_eq!(outputs.off_requests, [true, true]);
        for b in [Branch::A, Branch::B] {
            machine
                .confirm_off(token, b, OffEvidence::ReportedOff)
                .unwrap();
        }
        assert_eq!(machine.state(), MachineState::FaultLatched);
        assert_eq!(machine.output_state(), OutputState::OffVerified);
    }
}

#[test]
fn no_primary_proof_requests_fallback_and_does_not_claim_off() {
    let mut monitor = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = sample();
    i.source_connected = false;
    let reason = monitor
        .evaluate(&i)
        .faults
        .into_iter()
        .find(|r| *r == FaultReason::SourceLinkLost)
        .unwrap();
    let mut machine = running();
    let token = machine.latch_fault(reason);
    machine.proof_unavailable(token).unwrap();
    assert!(machine.fallback_requested());
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine.reset().is_err());
}

#[test]
fn successful_fallback_requires_new_independent_evidence() {
    let mut machine = running();
    let primary = machine.latch_fault(FaultReason::SourceLinkLost);
    machine.proof_unavailable(primary).unwrap();
    assert!(machine.fallback_requested());
    assert_eq!(machine.output_state(), OutputState::Unknown);
    // The synthetic fallback succeeds and a new independent observation proves both outputs off.
    let retry = machine.request_off().unwrap();
    for b in [Branch::A, Branch::B] {
        machine
            .confirm_off(retry, b, OffEvidence::MeasuredNearZero)
            .unwrap();
    }
    assert_eq!(machine.output_state(), OutputState::OffVerified);
    assert_eq!(machine.state(), MachineState::FaultLatched);
}

#[test]
fn warnings_and_derating_do_not_latch_a_fault() {
    for stage2 in [49.0, 50.0] {
        let mut monitor = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
        let mut i = sample();
        i.temperatures.as_mut().unwrap().stage2_k = Kelvin::new(stage2).unwrap();
        let result = monitor.evaluate(&i);
        assert!(result.faults.is_empty(), "{result:?}");
        assert!(!result.warnings.is_empty());
        assert_eq!(result.derate_power_w.is_some(), stage2 == 50.0);
        assert_eq!(running().state(), MachineState::Running);
    }
}

#[test]
fn reconnect_and_restart_never_resume() {
    let mut old = running();
    old.disconnect();
    old.connect().unwrap();
    assert_eq!(old.output_state(), OutputState::Unknown);
    assert!(old.start().is_err());
    assert!(old.arm().is_err());
    let mut restarted = SafetyMachine::new();
    restarted.connect().unwrap();
    assert_eq!(restarted.output_state(), OutputState::Unknown);
    assert!(restarted.start().is_err());
    assert!(restarted.arm().is_err());
}

#[test]
fn phase_power_limit_trips_only_for_the_selected_phase() {
    let mut profile = ProtectionProfile::test_only_example();
    profile.phase_power_limits_w[1] = Watts::new(1.5).unwrap();
    let mut input = sample(); // measured total is 2 W
    let cooling = SafetyMonitor::new(profile.clone())
        .unwrap()
        .evaluate(&input);
    assert!(!cooling.faults.contains(&FaultReason::HeadPower));
    input.cooling_phase = CoolingPhase::Stable;
    let stable = SafetyMonitor::new(profile).unwrap().evaluate(&input);
    assert!(stable.faults.contains(&FaultReason::HeadPower));
    let mut machine = running();
    let token = machine.latch_fault(FaultReason::HeadPower);
    let mut outputs = SyntheticOutputs::default();
    outputs.apply_off();
    assert_eq!(outputs.off_requests, [true, true]);
    for b in [Branch::A, Branch::B] {
        machine
            .confirm_off(token, b, OffEvidence::ReportedOff)
            .unwrap();
    }
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.output_state(), OutputState::OffVerified);
}

#[test]
fn elapsed_monotonic_tick_gap_trips_without_a_precomputed_flag() {
    let mut profile = ProtectionProfile::test_only_example();
    profile.max_tick_gap = Duration::from_secs(1);
    let mut monitor = SafetyMonitor::new(profile).unwrap();
    let mut input = sample();
    assert!(!monitor
        .evaluate(&input)
        .faults
        .contains(&FaultReason::MissedTick));
    input.monotonic += Duration::from_secs(2);
    input.wall_time_s = Seconds::new(input.wall_time_s.get() + 2.0).unwrap();
    input.temperatures.as_mut().unwrap().measured_at = input.monotonic;
    assert!(!input.missed_tick);
    assert!(monitor
        .evaluate(&input)
        .faults
        .contains(&FaultReason::MissedTick));
    let mut machine = running();
    let token = machine.latch_fault(FaultReason::MissedTick);
    for b in [Branch::A, Branch::B] {
        machine
            .confirm_off(token, b, OffEvidence::ReportedOff)
            .unwrap();
    }
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.output_state(), OutputState::OffVerified);
}
