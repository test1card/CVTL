use std::time::{Duration, Instant};

use cvtl_core::units::{Amperes, Ohms, Volts, Watts};
use cvtl_instruments::keithley::{Channel, ChannelConfig, Keithley, OutputState, SourceChannel};
use cvtl_instruments::{InstrumentError, LoopbackTcp, MessageTransport};
use cvtl_sim::keithley::KeithleySimulator;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

#[test]
fn simulator_rejects_a_query_outside_its_tsp_subset() {
    let sim = KeithleySimulator::start().unwrap();
    let mut transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    assert!(matches!(
        transport.query("print(smua.source.rangie)", deadline()),
        Err(InstrumentError::Disconnected)
    ));
    assert!(!transport.is_usable());
}
fn config() -> ChannelConfig {
    ChannelConfig {
        limit_v: Volts::new(10.0).unwrap(),
        limit_i: Amperes::new(1.0).unwrap(),
        limit_p: Watts::new(10.0).unwrap(),
        source_range_i: Amperes::new(1.0).unwrap(),
        measure_range_i: Amperes::new(1.0).unwrap(),
        measure_range_v: Volts::new(40.0).unwrap(),
        nplc: 0.1,
    }
}

#[test]
fn measured_values_follow_independent_load_and_channels_are_sequential() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    for channel in [Channel::A, Channel::B] {
        instrument.configure(channel, config(), deadline()).unwrap();
        instrument
            .set_current(channel, Amperes::new(0.2).unwrap(), deadline())
            .unwrap();
        instrument.output_on(channel, deadline()).unwrap();
    }
    sim.set_load(Channel::A, Ohms::new(10.0).unwrap()).unwrap();
    sim.set_load(Channel::B, Ohms::new(20.0).unwrap()).unwrap();
    let (a, b) = instrument.measure_pair(deadline()).unwrap();
    assert!((a.voltage_v.get() - 2.0).abs() < 1e-9);
    assert!((b.voltage_v.get() - 4.0).abs() < 1e-9);
    assert!(a.started <= a.finished && a.finished <= b.started && b.started <= b.finished);
    assert_eq!(a.nplc, 0.1);
    sim.set_load(Channel::A, Ohms::new(30.0).unwrap()).unwrap();
    let updated = instrument.measure(Channel::A, deadline()).unwrap();
    assert!((updated.voltage_v.get() - 6.0).abs() < 1e-9);
    assert!((updated.current_a.get() - 0.2).abs() < 1e-9);
}

#[test]
fn output_requires_verified_configuration_and_recovery_turns_both_off() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    assert!(instrument.output_on(Channel::A, deadline()).is_err());
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(), deadline())
        .unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    assert_eq!(
        instrument.output_state(Channel::A, deadline()).unwrap(),
        OutputState::On
    );
    instrument.recover(deadline()).unwrap();
    assert_eq!(
        instrument.output_state(Channel::A, deadline()).unwrap(),
        OutputState::Off
    );
    assert_eq!(
        instrument.output_state(Channel::B, deadline()).unwrap(),
        OutputState::Off
    );
    assert!(instrument.output_on(Channel::A, deadline()).is_err());
}

#[test]
fn recovery_checks_identity_before_sending_off_commands() {
    let sim = KeithleySimulator::start().unwrap();
    let connect = || LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(connect());
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(), deadline())
        .unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    drop(instrument);

    sim.override_next_query("*IDN?", b"KEITHLEY INSTRUMENTS INC.,MODEL 2612B,SIM,1.0\n");
    let mut wrong_device = Keithley::new(connect());
    assert!(wrong_device.recover(deadline()).is_err());

    let mut observer = connect();
    let output = observer
        .query("print(smua.source.output)", deadline())
        .unwrap();
    assert_eq!(
        cvtl_instruments::keithley::parse_number(&output).unwrap(),
        1.0
    );
}

#[test]
fn invalid_configuration_is_rejected() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    let mut bad = config();
    bad.nplc = f64::NAN;
    assert!(instrument.configure(Channel::A, bad, deadline()).is_err());
    assert!(instrument.output_on(Channel::A, deadline()).is_err());
}

#[test]
fn iv_parser_rejects_instrument_overflow_and_malformed_data() {
    use cvtl_instruments::keithley::parse_iv;
    assert_eq!(parse_iv(b"1.00000e-1\t2.00000e+0").unwrap(), (0.1, 2.0));
    for bytes in [
        &b"9.91e37\t1"[..],
        &b"1\t9.91e37"[..],
        &b"NaN\t1"[..],
        &b"1\t2\t3"[..],
        &b"\xff"[..],
    ] {
        assert!(parse_iv(bytes).is_err());
    }
}

#[test]
fn lost_session_makes_both_channels_unknown_and_requires_new_recovery() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(), deadline())
        .unwrap();
    instrument
        .configure(Channel::B, config(), deadline())
        .unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    sim.disconnect_next_query();
    assert!(instrument.measure(Channel::A, deadline()).is_err());
    for channel in [Channel::A, Channel::B] {
        assert_eq!(
            instrument.output_state(channel, deadline()).unwrap(),
            OutputState::Unknown
        );
        assert!(instrument.output_on(channel, deadline()).is_err());
    }
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut replacement = Keithley::new(transport);
    replacement.recover(deadline()).unwrap();
    assert_eq!(
        replacement.output_state(Channel::A, deadline()).unwrap(),
        OutputState::Off
    );
    assert_eq!(
        replacement.output_state(Channel::B, deadline()).unwrap(),
        OutputState::Off
    );
    assert!(replacement.output_on(Channel::A, deadline()).is_err());
}

#[test]
fn identity_error_and_iv_wire_formats_are_strict() {
    use cvtl_instruments::keithley::{parse_error, parse_identity, parse_iv};
    assert!(parse_identity(b"KEITHLEY INSTRUMENTS INC.,MODEL 2604B,1234,3.4.0").is_ok());
    // Real 2604B firmware 3.2.1 replies with mixed case.
    assert!(parse_identity(b"Keithley Instruments Inc., Model 2604B, 4083236, 3.2.1").is_ok());
    assert!(parse_identity(b"KEITHLEY,MODEL 2602B,1234,3.4.0").is_err());
    assert!(parse_identity(b"KEITHLEY,MODEL 2604B,1234,3.4.0\nextra").is_err());
    assert!(parse_error(b"0.00000e+00\tQueue Is Empty\t0.00000e+00\t1.00000e+00").is_ok());
    assert!(parse_error(b"1\tBad\t2.5\t1").is_err());
    assert!(parse_error(b"1e-9999\tBad\t0\t1").is_err());
    assert!(cvtl_instruments::keithley::parse_number(b"1e-9999").is_err());
    assert!(parse_error(b"0\tQueue Is Empty").is_err());
    for bad in [&b"1 2"[..], &b"1\n2"[..], &b"1\t2\n3"[..], &b"1\t 2"[..]] {
        assert!(parse_iv(bad).is_err());
    }
}

#[test]
fn readback_mismatch_invalidates_shared_session_without_retry() {
    for path in ["configuration", "current", "enable"] {
        let sim = KeithleySimulator::start().unwrap();
        let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
        let mut instrument = Keithley::new(transport);
        instrument.recover(deadline()).unwrap();
        if path != "configuration" {
            instrument
                .configure(Channel::A, config(), deadline())
                .unwrap();
        }
        let query = match path {
            "configuration" => "print(smua.source.limitv)",
            "current" => "print(smua.source.leveli)",
            _ => "print(smua.source.output)",
        };
        let before = sim.query_count(query);
        sim.override_next_query(query, b"0\n");
        let result = match path {
            "configuration" => instrument.configure(Channel::A, config(), deadline()),
            "current" => instrument.set_current(Channel::A, Amperes::new(0.2).unwrap(), deadline()),
            _ => instrument.output_on(Channel::A, deadline()),
        };
        assert!(result.is_err(), "{path}");
        assert_eq!(sim.query_count(query), before + 1, "{path}");
        for channel in [Channel::A, Channel::B] {
            assert_eq!(
                instrument.output_state(channel, deadline()).unwrap(),
                OutputState::Unknown
            );
            assert!(instrument.output_on(channel, deadline()).is_err());
        }
    }
}

#[test]
fn simulated_voltage_compliance_changes_measured_current() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(), deadline())
        .unwrap();
    instrument
        .set_current(Channel::A, Amperes::new(0.2).unwrap(), deadline())
        .unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    sim.set_load(Channel::A, Ohms::new(100.0).unwrap()).unwrap();
    let measured = instrument.measure(Channel::A, deadline()).unwrap();
    assert!((measured.voltage_v.get() - 10.).abs() < 1e-9);
    assert!((measured.current_a.get() - 0.1).abs() < 1e-9);
}

#[test]
fn rejects_unconfirmed_ranges_and_control_bytes() {
    use cvtl_instruments::keithley::{parse_error, parse_identity, parse_number};
    assert!(parse_number(b"0\n").is_err());
    assert!(parse_identity(b"XKEITHLEY INSTRUMENTS INC.,MODEL 2604B,1,1").is_err());
    assert!(parse_identity(b"KEITHLEY INSTRUMENTS INC.,MODEL 2604B,1,1\0").is_err());
    assert!(parse_error(b"0\tQueue\0 Is Empty\t0\t1").is_err());
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    let mut c = config();
    c.measure_range_v = Volts::new(10.).unwrap();
    assert!(instrument.configure(Channel::A, c, deadline()).is_err());
    c.source_range_i = Amperes::new(3.).unwrap();
    c.measure_range_i = Amperes::new(3.).unwrap();
    c.measure_range_v = Volts::new(40.).unwrap();
    assert!(instrument.configure(Channel::A, c, deadline()).is_err());
    c.measure_range_v = Volts::new(6.).unwrap();
    assert!(instrument.configure(Channel::A, c, deadline()).is_err());
    c = config();
    c.limit_p = Watts::new(41.).unwrap();
    assert!(instrument.configure(Channel::A, c, deadline()).is_err());
}
