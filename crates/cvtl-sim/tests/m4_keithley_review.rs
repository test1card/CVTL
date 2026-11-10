//! Regression checks of the simulated instrument and strict protocol boundaries.
use cvtl_core::units::{Amperes, Ohms, Volts, Watts};
use cvtl_instruments::keithley::{
    parse_identity, parse_iv, Channel, ChannelConfig, Keithley, OutputState, SourceChannel,
};
use cvtl_instruments::LoopbackTcp;
use cvtl_sim::keithley::KeithleySimulator;
use std::time::{Duration, Instant};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn config(power: f64) -> ChannelConfig {
    ChannelConfig {
        limit_v: Volts::new(10.0).unwrap(),
        limit_i: Amperes::new(1.0).unwrap(),
        limit_p: Watts::new(power).unwrap(),
        source_range_i: Amperes::new(1.0).unwrap(),
        measure_range_i: Amperes::new(1.0).unwrap(),
        measure_range_v: Volts::new(40.0).unwrap(),
        nplc: 0.1,
    }
}

#[test]
fn zero_current_has_zero_iv_for_both_output_states() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(1.0), deadline())
        .unwrap();
    instrument
        .set_current(Channel::A, Amperes::new(0.0).unwrap(), deadline())
        .unwrap();
    sim.set_load(Channel::A, Ohms::new(10.0).unwrap()).unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    assert_eq!(
        instrument.output_state(Channel::A, deadline()).unwrap(),
        OutputState::On
    );
    let on = instrument.measure(Channel::A, deadline()).unwrap();
    assert_eq!((on.current_a.get(), on.voltage_v.get()), (0.0, 0.0));
    instrument.output_off(Channel::A, deadline()).unwrap();
    assert_eq!(
        instrument.output_state(Channel::A, deadline()).unwrap(),
        OutputState::Off
    );
    let off = instrument.measure(Channel::A, deadline()).unwrap();
    assert_eq!((off.current_a.get(), off.voltage_v.get()), (0.0, 0.0));
}

#[test]
fn power_limit_acts_before_voltage_limit() {
    let sim = KeithleySimulator::start().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(1)).unwrap();
    let mut instrument = Keithley::new(transport);
    instrument.recover(deadline()).unwrap();
    instrument
        .configure(Channel::A, config(1.0), deadline())
        .unwrap();
    sim.set_load(Channel::A, Ohms::new(10.0).unwrap()).unwrap();
    instrument
        .set_current(Channel::A, Amperes::new(0.5).unwrap(), deadline())
        .unwrap();
    instrument.output_on(Channel::A, deadline()).unwrap();
    let measured = instrument.measure(Channel::A, deadline()).unwrap();
    let i = measured.current_a.get();
    let v = measured.voltage_v.get();
    assert!((i - (0.1_f64).sqrt()).abs() < 1e-9);
    assert!(v < 10.0);
    assert!((v * i - 1.0).abs() < 1e-9);
}

#[test]
fn strict_iv_and_identity_formats() {
    for bytes in [
        &b"1\t2 "[..],
        &b"1\t2\t"[..],
        &b"1\t2\n"[..],
        &b"1\t2\r"[..],
    ] {
        assert!(parse_iv(bytes).is_err(), "accepted {bytes:?}");
    }
    for model in ["2604B", "MODEL 2604B"] {
        assert!(
            parse_identity(format!("KEITHLEY INSTRUMENTS INC.,{model},1234,1.0").as_bytes())
                .is_ok()
        );
    }
    assert!(parse_identity(b"KEITHLEY INSTRUMENTS INC.,MODEL 2601B,1234,1.0").is_err());
}
