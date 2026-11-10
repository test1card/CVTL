use cvtl_core::units::{Amperes, Kelvin, Seconds, Volts, Watts};
use cvtl_safety::monitor::*;
use cvtl_safety::{Branch, FaultReason};
use std::time::Duration;

fn input() -> MonitorInput {
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
        allow_stale_once: true,
        source_connected: true,
        thermometer_connected: true,
        missed_tick: false,
        expect_tick: true,
        queue_fault: false,
        model_valid: true,
        journal_ok: true,
        cooling_phase: CoolingPhase::Cooling,
    }
}

#[test]
fn hard_conditions_are_named_and_thresholds_validated() {
    let p = ProtectionProfile::test_only_example();
    assert!(p.clone().validate().is_ok());
    let mut bad = p.clone();
    bad.max_total_power_w = Watts::new(-1.0).unwrap();
    assert!(bad.validate().is_err());
    let mut m = SafetyMonitor::new(p).unwrap();
    let mut i = input();
    i.branches[0].as_mut().unwrap().current_a = Amperes::new(11.0).unwrap();
    assert!(m
        .evaluate(&i)
        .faults
        .contains(&FaultReason::BranchCurrent(Branch::A)));
    i = input();
    i.source_connected = false;
    i.journal_ok = false;
    let o = m.evaluate(&i);
    assert!(o.faults.contains(&FaultReason::SourceLinkLost));
    assert!(o.faults.contains(&FaultReason::JournalFailure));
}

#[test]
fn tracking_duration_uses_monotonic_time() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    i.branches[0].as_mut().unwrap().command_power_w = Watts::new(4.0).unwrap();
    assert!(!m
        .evaluate(&i)
        .faults
        .contains(&FaultReason::PowerMismatch(Branch::A)));
    i.monotonic += Duration::from_secs(3);
    i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
    assert!(m
        .evaluate(&i)
        .faults
        .contains(&FaultReason::PowerMismatch(Branch::A)));
}

#[test]
fn head_warning_derate_and_trip_are_distinct() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    i.temperatures.as_mut().unwrap().stage2_k = Kelvin::new(49.0).unwrap();
    let o = m.evaluate(&i);
    assert!(o.warnings.contains(&Warning::HeadApproaching));
    assert!(o.faults.is_empty());
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(i.wall_time_s.get() + 1.0).unwrap();
    i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
    i.temperatures.as_mut().unwrap().stage2_k = Kelvin::new(50.0).unwrap();
    let o = m.evaluate(&i);
    assert!(o.derate_power_w.is_some());
    assert!(o.faults.is_empty());
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(i.wall_time_s.get() + 1.0).unwrap();
    i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
    i.temperatures.as_mut().unwrap().stage2_k = Kelvin::new(51.0).unwrap();
    assert!(m.evaluate(&i).faults.contains(&FaultReason::HeadInversion));
}

#[test]
fn detects_each_external_fault_and_missing_inputs() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    i.thermometer_connected = false;
    i.missed_tick = true;
    i.queue_fault = true;
    i.model_valid = false;
    i.temperatures = None;
    i.branches[1] = None;
    let o = m.evaluate(&i);
    for cause in [
        FaultReason::ThermometerLinkLost,
        FaultReason::MissedTick,
        FaultReason::InstrumentQueue,
        FaultReason::ModelFailure,
        FaultReason::MeasurementUnavailable,
    ] {
        assert!(o.faults.contains(&cause));
    }
}

#[test]
fn voltage_branch_power_total_and_mapping_are_independent() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    i.branches[0].as_mut().unwrap().voltage_v = Volts::new(11.0).unwrap();
    i.branches[0].as_mut().unwrap().current_a = Amperes::new(2.0).unwrap();
    i.branches[1].as_mut().unwrap().current_a = Amperes::new(5.0).unwrap();
    i.temperatures.as_mut().unwrap().stage1_channel = 2;
    let o = m.evaluate(&i);
    for cause in [
        FaultReason::BranchVoltage(Branch::A),
        FaultReason::BranchPower(Branch::A),
        FaultReason::TotalPower,
        FaultReason::ChannelMismatch,
    ] {
        assert!(o.faults.contains(&cause));
    }
}

#[test]
fn one_stale_sample_warns_then_faults_and_recovery_is_fresh() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    assert!(m.evaluate(&i).faults.is_empty());
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1001.0).unwrap();
    assert!(m.evaluate(&i).faults.is_empty());
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1002.0).unwrap();
    assert!(m.evaluate(&i).warnings.contains(&Warning::MeasurementStale));
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1003.0).unwrap();
    assert!(m
        .evaluate(&i)
        .faults
        .contains(&FaultReason::TemperatureStale));
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1004.0).unwrap();
    i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
    assert!(m.evaluate(&i).faults.is_empty());
}

#[test]
fn wall_jump_and_temperature_rates_are_detected() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    m.evaluate(&i);
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1100.0).unwrap();
    let t = i.temperatures.as_mut().unwrap();
    t.measured_at = i.monotonic;
    t.stage1_k = Kelvin::new(200.0).unwrap();
    let o = m.evaluate(&i);
    assert!(o.faults.contains(&FaultReason::WallClockJump));
    assert!(o.faults.contains(&FaultReason::HeadRate));
}

#[test]
fn swapped_branches_and_long_tick_are_detected() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    m.evaluate(&i);
    i.branch_channels = [Branch::B, Branch::A];
    i.monotonic += Duration::from_secs(2);
    i.wall_time_s = Seconds::new(1002.0).unwrap();
    let o = m.evaluate(&i);
    assert!(o.faults.contains(&FaultReason::ChannelMismatch));
    assert!(o.faults.contains(&FaultReason::MissedTick));
}

#[test]
fn phase_cap_is_hard_but_soft_derating_is_not() {
    let mut p = ProtectionProfile::test_only_example();
    p.phase_power_limits_w = [
        Watts::new(3.0).unwrap(),
        Watts::new(1.5).unwrap(),
        Watts::new(3.0).unwrap(),
    ];
    let mut m = SafetyMonitor::new(p).unwrap();
    let mut i = input();
    assert!(!m.evaluate(&i).faults.contains(&FaultReason::HeadPower));
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1001.0).unwrap();
    i.temperatures.as_mut().unwrap().measured_at = i.monotonic;
    i.cooling_phase = CoolingPhase::Stable;
    assert!(m.evaluate(&i).faults.contains(&FaultReason::HeadPower));
}

#[test]
fn backwards_temperature_does_not_replace_history() {
    let mut m = SafetyMonitor::new(ProtectionProfile::test_only_example()).unwrap();
    let mut i = input();
    m.evaluate(&i);
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1001.0).unwrap();
    let t = i.temperatures.as_mut().unwrap();
    t.measured_at = Duration::ZERO;
    t.stage1_k = Kelvin::new(200.0).unwrap();
    assert!(m.evaluate(&i).faults.contains(&FaultReason::MonotonicClock));
    i.monotonic += Duration::from_secs(1);
    i.wall_time_s = Seconds::new(1002.0).unwrap();
    let t = i.temperatures.as_mut().unwrap();
    t.measured_at = i.monotonic;
    t.stage1_k = Kelvin::new(51.0).unwrap();
    assert!(!m.evaluate(&i).faults.contains(&FaultReason::HeadRate));
}
