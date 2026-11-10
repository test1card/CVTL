use cvtl_core::units::{JoulesPerKelvin, Kelvin, Seconds, Watts, WattsPerKelvin};
use cvtl_sim::head::{HeadParameters, HeadTemperatures, SyntheticHead};

fn k(value: f64) -> Kelvin {
    Kelvin::new(value).unwrap()
}

fn w(value: f64) -> Watts {
    Watts::new(value).unwrap()
}

fn s(value: f64) -> Seconds {
    Seconds::new(value).unwrap()
}

fn parameters(stage_link: f64, fixture_link: f64) -> HeadParameters {
    HeadParameters {
        stage1_capacity: JoulesPerKelvin::new(10.0).unwrap(),
        stage2_capacity: JoulesPerKelvin::new(20.0).unwrap(),
        fixture_capacity: JoulesPerKelvin::new(5.0).unwrap(),
        stage_link: WattsPerKelvin::new(stage_link).unwrap(),
        fixture_link: WattsPerKelvin::new(fixture_link).unwrap(),
    }
}

fn initial() -> HeadTemperatures {
    HeadTemperatures {
        stage1: k(40.0),
        stage2: k(10.0),
        tf: k(12.0),
    }
}

fn energy_change(after: HeadTemperatures, before: HeadTemperatures) -> f64 {
    10.0 * (after.stage1.get() - before.stage1.get())
        + 20.0 * (after.stage2.get() - before.stage2.get())
        + 5.0 * (after.tf.get() - before.tf.get())
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() <= 1e-9, "{actual} != {expected}");
}

#[test]
fn thermal_uncoupled_matches_constant_power_analytic_solution() {
    let mut model = SyntheticHead::new(parameters(0.0, 0.0), initial()).unwrap();
    for _ in 0..4 {
        model
            .advance(s(0.5), [w(1.0), w(2.0)], [w(2.0), w(4.0)])
            .unwrap();
    }
    let state = model.temperatures();
    close(state.stage1.get(), 39.6);
    close(state.stage2.get(), 9.6);
    close(state.tf.get(), 13.2);
    close(energy_change(state, initial()), -6.0);
}

#[test]
fn thermal_links_move_heat_to_stage2_and_conserve_energy() {
    let mut model = SyntheticHead::new(parameters(0.5, 1.0), initial()).unwrap();
    model.advance(s(0.1), [w(0.0); 2], [w(0.0); 2]).unwrap();
    let state = model.temperatures();
    close(state.stage1.get(), 39.85);
    close(state.stage2.get(), 10.085);
    close(state.tf.get(), 11.96);
    close(energy_change(state, initial()), 0.0);
}

#[test]
fn thermal_invalid_steps_and_parameters_are_rejected_atomically() {
    let mut bad = parameters(0.5, 1.0);
    bad.fixture_capacity = JoulesPerKelvin::new(0.0).unwrap();
    assert!(SyntheticHead::new(bad, initial()).is_err());
    assert!(SyntheticHead::new(parameters(-0.1, 1.0), initial()).is_err());
    assert!(SyntheticHead::new(
        parameters(0.5, 1.0),
        HeadTemperatures {
            tf: k(-1.0),
            ..initial()
        }
    )
    .is_err());

    let mut model = SyntheticHead::new(parameters(0.5, 1.0), initial()).unwrap();
    for (dt, heat, cooling) in [
        (s(0.0), [w(0.0); 2], [w(0.0); 2]),
        (s(-1.0), [w(0.0); 2], [w(0.0); 2]),
        (s(5.01), [w(0.0); 2], [w(0.0); 2]),
        (s(0.1), [w(-1.0), w(0.0)], [w(0.0); 2]),
        (s(0.1), [w(0.0); 2], [w(0.0), w(-1.0)]),
        (s(0.1), [w(f64::MAX); 2], [w(0.0); 2]),
        (s(1.0), [w(0.0); 2], [w(0.0), w(1000.0)]),
    ] {
        assert!(model.advance(dt, heat, cooling).is_err());
        assert_eq!(model.temperatures(), initial());
    }
}

#[test]
fn loopback_head_consumes_actual_power_and_supplies_channel_temperatures() {
    use cvtl_core::units::{Amperes, Volts};
    use cvtl_instruments::{
        keithley::{Channel as Source, ChannelConfig, Keithley, SourceChannel},
        lakeshore::{Channel as Sensor, LakeShore218, SensorUnit, Thermometer},
        LoopbackTcp,
    };
    use cvtl_sim::{keithley::KeithleySimulator, lakeshore::LakeShoreSim};
    use std::time::{Duration, Instant};

    let timeout = Duration::from_secs(2);
    let deadline = || Instant::now() + timeout;
    let source = KeithleySimulator::start().unwrap();
    let mut driver = Keithley::new(LoopbackTcp::connect(source.addr(), timeout).unwrap());
    driver.recover(deadline()).unwrap();
    let config = ChannelConfig {
        limit_v: Volts::new(1.0).unwrap(),
        limit_i: Amperes::new(1.0).unwrap(),
        limit_p: w(0.25),
        source_range_i: Amperes::new(1.0).unwrap(),
        measure_range_i: Amperes::new(1.0).unwrap(),
        measure_range_v: Volts::new(40.0).unwrap(),
        nplc: 0.1,
    };
    for (channel, current) in [(Source::A, 0.05), (Source::B, 0.2)] {
        driver.configure(channel, config, deadline()).unwrap();
        driver
            .set_current(channel, Amperes::new(current).unwrap(), deadline())
            .unwrap();
    }
    assert_eq!(source.delivered_power().unwrap(), [w(0.0); 2]);
    for measurement in
        [Source::A, Source::B].map(|channel| driver.measure(channel, deadline()).unwrap())
    {
        assert_eq!(
            measurement.current_a.get() * measurement.voltage_v.get(),
            0.0
        );
    }
    for channel in [Source::A, Source::B] {
        driver.output_on(channel, deadline()).unwrap();
    }

    let delivered = source.delivered_power().unwrap();
    for (actual, channel) in delivered.into_iter().zip([Source::A, Source::B]) {
        let measurement = driver.measure(channel, deadline()).unwrap();
        close(
            actual.get(),
            measurement.current_a.get() * measurement.voltage_v.get(),
        );
    }
    close(delivered[0].get(), 0.025);
    close(delivered[1].get(), 0.1);
    source.disconnect_next_query();
    assert!(driver.measure(Source::A, deadline()).is_err());
    assert_eq!(source.delivered_power().unwrap(), delivered);

    let mut head = SyntheticHead::new(parameters(0.0, 0.0), initial()).unwrap();
    head.advance(s(1.0), source.delivered_power().unwrap(), [w(2.0), w(4.0)])
        .unwrap();
    let temperatures = head.temperatures();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let mut readback =
        LakeShore218::new(LoopbackTcp::connect(thermometer.addr(), timeout).unwrap());
    let mut read = |n| {
        readback
            .read(Sensor::new(n).unwrap(), SensorUnit::Volts, deadline())
            .unwrap()
            .kelvin
    };
    assert_eq!(read(1), k(42.0));
    let channels = [
        temperatures.stage1,
        temperatures.stage2,
        temperatures.tf,
        k(77.0),
        k(42.0),
        k(42.0),
        k(42.0),
        k(42.0),
    ];
    thermometer.set_temperatures(channels).unwrap();
    let mut invalid = channels;
    invalid[0] = k(-1.0);
    assert!(thermometer.set_temperatures(invalid).is_err());
    for (channel, expected) in [(1, 39.8), (2, 9.8), (3, 12.025), (4, 77.0)] {
        close(read(channel).get(), expected);
    }

    drop(driver);
    let mut recovered = Keithley::new(LoopbackTcp::connect(source.addr(), timeout).unwrap());
    recovered.recover(deadline()).unwrap();
    assert_eq!(source.delivered_power().unwrap(), [w(0.0); 2]);
}
