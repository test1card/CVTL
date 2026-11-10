use std::time::{Duration, Instant};

use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    ControlConfig, Controller, Demand, HeatPermit, Tick, TimedPowerSample,
};
use cvtl_core::units::{Amperes, Kelvin, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_instruments::{
    keithley::{
        Channel, ChannelConfig, Keithley, Measurement, OutputState as Reported, SourceChannel,
    },
    LoopbackTcp, MessageTransport,
};
use cvtl_safety::{
    machine::{MachineState, OffEvidence, OutputState, RequestToken},
    monitor::{BranchReading, CoolingPhase, MonitorInput, ProtectionProfile, Temperatures},
    supervisor::{Effect, JournalError, OperatorAction, SafetyJournal, SafetyRecord, Supervisor},
    Branch, FaultReason,
};
use cvtl_sim::keithley::KeithleySimulator;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn v(x: f64) -> Volts {
    Volts::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn k(x: f64) -> Kelvin {
    Kelvin::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}

fn connect(sim: &KeithleySimulator) -> Keithley<LoopbackTcp> {
    let mut driver =
        Keithley::new(LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap());
    driver.recover(deadline()).unwrap();
    driver
}

fn apply_off(
    supervisor: &mut Supervisor,
    instrument: &mut Keithley<LoopbackTcp>,
    request: RequestToken,
) {
    if instrument.disable_both(deadline()).is_ok() {
        for branch in Branch::ALL {
            supervisor
                .confirm_off(request, branch, OffEvidence::ReportedOff)
                .unwrap();
        }
    } else {
        supervisor.proof_unavailable(request).unwrap();
    }
}

fn off_request(effect: Effect) -> RequestToken {
    let Effect::DisableBoth(request) = effect else {
        panic!("expected OFF")
    };
    request
}

fn controller() -> Controller {
    let spec = ChannelSpec {
        resistance: ResistanceRange::new(r(10.0), r(11.0)).unwrap(),
        loop_resistance: r(0.0),
        heater_limit: Some(w(4.0)),
    };
    let pi = PiConfig {
        kp: 0.3,
        ki: PerSecond::new(1.0).unwrap(),
        integral_limit: w(2.0),
        resistance_initial: r(10.0),
        resistance_min: r(10.0),
        resistance_max: r(11.0),
        resistance_tau: s(0.3),
        min_current: a(0.001),
        current_quantum: None,
        derivative: None,
    };
    let cfg = ControlConfig {
        pi: [pi; 2],
        branches: [spec; 2],
        margins: Margins::new(0.9, 0.9).unwrap(),
        total_limit: w(6.0),
        max_sample_age: s(0.5),
        temperature_hold: None,
    };
    let rate = WattsPerSecond::new(10.0).unwrap();
    Controller::new(
        cfg,
        Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap(),
    )
    .unwrap()
}

fn monitor_frame(step: u64, measured: Option<[Measurement; 2]>) -> MonitorInput {
    let monotonic = Duration::from_millis(1000 + 100 * step);
    let readings = match measured {
        Some(values) => values.map(|m| {
            Some(BranchReading {
                current_a: m.current_a,
                voltage_v: m.voltage_v,
                command_power_w: w(0.0),
            })
        }),
        None => [None; 2],
    };
    MonitorInput {
        monotonic,
        wall_time_s: s(1000.0 + monotonic.as_secs_f64()),
        branches: readings,
        branch_channels: Branch::ALL,
        temperatures: Some(Temperatures {
            stage1_k: k(50.0),
            stage2_k: k(40.0),
            measured_at: monotonic,
            stage1_channel: 1,
            stage2_channel: 2,
        }),
        source_connected: measured.is_some(),
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
}
impl SafetyJournal for Journal {
    fn record(&mut self, _: SafetyRecord<'_>) -> Result<(), JournalError> {
        if self.fail {
            Err(JournalError)
        } else {
            Ok(())
        }
    }
}

fn read(driver: &mut Keithley<LoopbackTcp>) -> [Measurement; 2] {
    let (a, b) = driver.measure_pair(deadline()).unwrap();
    [a, b]
}

fn configure(instrument: &mut Keithley<LoopbackTcp>) {
    let cfg = ChannelConfig {
        limit_v: v(10.0),
        limit_i: a(1.0),
        limit_p: w(4.0),
        source_range_i: a(1.0),
        measure_range_i: a(1.0),
        measure_range_v: v(40.0),
        nplc: 1.0,
    };
    for channel in [Channel::A, Channel::B] {
        instrument.configure(channel, cfg, deadline()).unwrap();
    }
}

fn running(sim: &KeithleySimulator, journal: &mut Journal) -> (Supervisor, Keithley<LoopbackTcp>) {
    let mut instrument = connect(sim);
    configure(&mut instrument);
    let mut supervisor = Supervisor::new(ProtectionProfile::test_only_example()).unwrap();
    supervisor.connect().unwrap();
    let request = off_request(supervisor.request_off().unwrap());
    apply_off(&mut supervisor, &mut instrument, request);
    assert_eq!(supervisor.state(), MachineState::OutputOffVerified);
    let off = read(&mut instrument);
    supervisor
        .operate(OperatorAction::Arm, &monitor_frame(0, Some(off)), journal)
        .unwrap();
    let start = supervisor
        .operate(OperatorAction::Start, &monitor_frame(1, Some(off)), journal)
        .unwrap();
    let Some(Effect::EnableBoth(token)) = start.effect else {
        panic!("expected ON")
    };
    for (branch, channel) in Branch::ALL.into_iter().zip([Channel::A, Channel::B]) {
        assert!(supervisor.can_enable(token));
        instrument.output_on(channel, deadline()).unwrap();
        assert_eq!(
            instrument.output_state(channel, deadline()).unwrap(),
            Reported::On
        );
        supervisor.confirm_on(token, branch).unwrap();
    }
    assert_eq!(supervisor.state(), MachineState::Running);
    assert!(!supervisor.can_enable(token));
    (supervisor, instrument)
}

fn tick_for(frame: &MonitorInput, measured: [Measurement; 2]) -> Tick {
    let at = s(frame.monotonic.as_secs_f64());
    Tick {
        demand: Demand::Power(w(1.0)),
        permit: HeatPermit::Inhibit,
        branches_available: [true; 2],
        measurements: measured.map(|m| {
            Some(TimedPowerSample {
                at,
                sample: PowerSample {
                    current: m.current_a,
                    voltage: m.voltage_v,
                },
            })
        }),
        now: at,
        dt: s(0.1),
    }
}

#[test]
fn tcp_loopback_obeys_permit_loss_and_reconnect_requires_off_and_new_arm() {
    let sim = KeithleySimulator::start().unwrap();
    let mut journal = Journal::default();
    let (mut supervisor, mut instrument) = running(&sim, &mut journal);
    let measured = read(&mut instrument);
    let frame = monitor_frame(2, Some(measured));
    assert!(matches!(
        supervisor.observe(&frame, &mut journal).permit,
        HeatPermit::Limit(_)
    ));
    let result = supervisor.regulate(
        frame.monotonic,
        &mut controller(),
        tick_for(&frame, measured),
        &mut journal,
    );
    let currents = result.currents.unwrap();
    assert!(currents.iter().all(|i| *i > a(0.0)));
    for (channel, current) in [Channel::A, Channel::B].into_iter().zip(currents) {
        instrument
            .set_current(channel, current, deadline())
            .unwrap();
    }
    assert!(read(&mut instrument).iter().all(|m| m.current_a > a(0.0)));

    sim.disconnect_next_query();
    assert!(instrument.measure_pair(deadline()).is_err());
    let lost = supervisor.observe(&monitor_frame(3, None), &mut journal);
    assert!(matches!(lost.permit, HeatPermit::Inhibit));
    let off = off_request(lost.effect.unwrap());
    apply_off(&mut supervisor, &mut instrument, off);
    assert_eq!(supervisor.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert_eq!(supervisor.state(), MachineState::FaultLatched);
    assert_eq!(supervisor.output_state(), OutputState::Unknown);
    assert!(supervisor.fallback_requested());

    supervisor.disconnect();
    let mut instrument = connect(&sim);
    supervisor.connect().unwrap();
    let request = off_request(supervisor.request_off().unwrap());
    assert!(supervisor
        .confirm_off(off, Branch::A, OffEvidence::ReportedOff)
        .is_err());
    apply_off(&mut supervisor, &mut instrument, request);
    assert_eq!(supervisor.state(), MachineState::FaultLatched);
    assert_eq!(supervisor.output_state(), OutputState::OffVerified);
    configure(&mut instrument);
    let after_reconnect = read(&mut instrument);
    supervisor
        .operate(
            OperatorAction::Reset,
            &monitor_frame(4, Some(after_reconnect)),
            &mut journal,
        )
        .unwrap();
    let observed = supervisor.observe(&monitor_frame(5, Some(read(&mut instrument))), &mut journal);
    assert!(matches!(observed.permit, HeatPermit::Inhibit));
    assert!(observed.effect.is_none());
    assert_eq!(supervisor.state(), MachineState::OutputOffVerified);
}

#[test]
fn tcp_loopback_journal_failure_withholds_drive_and_off_is_verified() {
    let sim = KeithleySimulator::start().unwrap();
    let mut journal = Journal::default();
    let (mut supervisor, mut instrument) = running(&sim, &mut journal);
    let measured = read(&mut instrument);
    let frame = monitor_frame(2, Some(measured));
    supervisor.observe(&frame, &mut journal);
    journal.fail = true;
    let output = supervisor.regulate(
        frame.monotonic,
        &mut controller(),
        tick_for(&frame, measured),
        &mut journal,
    );
    assert!(output.currents.is_none());
    let off = off_request(output.effect.unwrap());
    apply_off(&mut supervisor, &mut instrument, off);
    assert_eq!(supervisor.state(), MachineState::FaultLatched);
    assert_eq!(supervisor.fault_reason(), Some(FaultReason::JournalFailure));
    assert_eq!(supervisor.output_state(), OutputState::OffVerified);
}

#[test]
fn both_off_commands_precede_any_readback_even_if_a_fails() {
    let sim = KeithleySimulator::start().unwrap();
    let mut journal = Journal::default();
    let (_, mut instrument) = running(&sim, &mut journal);
    sim.override_next_query("print(smua.source.output)", b"1\n");
    assert!(instrument.disable_both(deadline()).is_err());
    let mut raw = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let b = raw.query("print(smub.source.output)", deadline()).unwrap();
    assert_eq!(cvtl_instruments::keithley::parse_number(&b).unwrap(), 0.0);
}
