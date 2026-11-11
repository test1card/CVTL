use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use cvtl_core::{
    units::{Amperes, Ohms, Volts, Watts},
    Config,
};
use cvtl_instruments::{
    keithley::{Channel, ChannelConfig, OutputState, SourceChannel},
    maisheng::MaishengStub,
    InstrumentError,
};
use cvtl_sim::maisheng::MaishengSim;

fn config() -> ChannelConfig {
    ChannelConfig {
        limit_v: Volts::new(10.0).unwrap(),
        limit_i: Amperes::new(1.0).unwrap(),
        limit_p: Watts::new(2.0).unwrap(),
        source_range_i: Amperes::new(1.0).unwrap(),
        measure_range_i: Amperes::new(1.0).unwrap(),
        measure_range_v: Volts::new(10.0).unwrap(),
        nplc: 1.0,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn unverified<T>(result: Result<T, InstrumentError>) {
    assert!(matches!(result, Err(InstrumentError::Unverified)));
}

fn timeout<T>(result: Result<T, InstrumentError>) {
    assert!(matches!(result, Err(InstrumentError::Timeout)));
}

fn invalid<T>(result: Result<T, InstrumentError>) {
    assert!(matches!(result, Err(InstrumentError::InvalidInput(_))));
}

fn new_source() -> MaishengSim {
    MaishengSim::new_test_only([Ohms::new(10.0).unwrap(), Ohms::new(20.0).unwrap()]).unwrap()
}

fn assert_sample(source: &mut impl SourceChannel, channel: Channel, current: f64, voltage: f64) {
    let until = deadline();
    let sample = source.measure(channel, until).unwrap();
    assert_eq!(sample.current_a.get(), current);
    assert_eq!(sample.voltage_v.get(), voltage);
    assert_eq!(sample.nplc, 1.0);
    assert!(sample.started <= sample.finished && sample.finished <= until);
}

#[test]
fn unverified_stub_never_claims_measurement_identity_or_output() {
    let mut source = MaishengStub;
    for channel in [Channel::A, Channel::B] {
        for expiry in [deadline(), Instant::now()] {
            unverified(source.configure(channel, config(), expiry));
            unverified(source.set_current(channel, Amperes::new(0.25).unwrap(), expiry));
            unverified(source.measure(channel, expiry));
            unverified(source.output_on(channel, expiry));
            unverified(source.output_off(channel, expiry));
            unverified(source.output_state(channel, expiry));
            unverified(source.errors(expiry));
            unverified(source.identity(expiry));
        }
    }
}

#[test]
fn synthetic_source_starts_off_without_configuration() {
    let mut source = new_source();
    for channel in [Channel::A, Channel::B] {
        assert_eq!(
            source.output_state(channel, deadline()).unwrap(),
            OutputState::Off
        );
        assert!(matches!(
            source.output_on(channel, deadline()),
            Err(InstrumentError::State(_))
        ));
        assert!(matches!(
            source.measure(channel, deadline()),
            Err(InstrumentError::State(_))
        ));
        assert!(matches!(
            source.set_current(channel, Amperes::new(0.25).unwrap(), deadline()),
            Err(InstrumentError::State(_))
        ));
        source.output_off(channel, deadline()).unwrap();
    }
    assert!(source.errors(deadline()).unwrap().is_empty());
    assert!(source.identity(deadline()).unwrap().contains("test_only"));
}

#[test]
fn synthetic_measurement_follows_load_and_off_clears_the_preset() {
    let mut source = new_source();
    for (channel, current) in [(Channel::A, 0.25), (Channel::B, 0.1)] {
        source.configure(channel, config(), deadline()).unwrap();
        source
            .set_current(channel, Amperes::new(current).unwrap(), deadline())
            .unwrap();
        assert_sample(&mut source, channel, 0.0, 0.0);
        source.output_on(channel, deadline()).unwrap();
    }
    assert_sample(&mut source, Channel::A, 0.25, 2.5);
    let first = source.measure(Channel::A, deadline()).unwrap();
    assert_eq!(first.current_a.get() * first.voltage_v.get(), 0.625);
    source
        .set_resistance(Channel::A, Ohms::new(20.0).unwrap())
        .unwrap();
    assert_sample(&mut source, Channel::A, 0.25, 5.0);
    let second = source.measure(Channel::A, deadline()).unwrap();
    assert_eq!(second.current_a.get() * second.voltage_v.get(), 1.25);
    assert_sample(&mut source, Channel::B, 0.1, 2.0);

    source.output_off(Channel::A, deadline()).unwrap();
    assert_eq!(
        source.output_state(Channel::A, deadline()).unwrap(),
        OutputState::Off
    );
    assert_eq!(
        source.output_state(Channel::B, deadline()).unwrap(),
        OutputState::On
    );
    assert_sample(&mut source, Channel::A, 0.0, 0.0);
    assert_sample(&mut source, Channel::B, 0.1, 2.0);
    source.output_on(Channel::A, deadline()).unwrap();
    assert_sample(&mut source, Channel::A, 0.0, 0.0);

    source.output_off(Channel::A, deadline()).unwrap();
    source
        .set_current(Channel::A, Amperes::new(0.2).unwrap(), deadline())
        .unwrap();
    source.configure(Channel::A, config(), deadline()).unwrap();
    source.output_on(Channel::A, deadline()).unwrap();
    assert_sample(&mut source, Channel::A, 0.0, 0.0);
}

#[test]
fn rejected_current_or_load_keeps_the_previous_operating_point() {
    let mut source = new_source();
    source.configure(Channel::A, config(), deadline()).unwrap();
    source
        .set_current(Channel::A, Amperes::new(0.25).unwrap(), deadline())
        .unwrap();
    source.output_on(Channel::A, deadline()).unwrap();
    for current in [-0.1, 1.1, 0.5] {
        invalid(source.set_current(Channel::A, Amperes::new(current).unwrap(), deadline()));
        assert_sample(&mut source, Channel::A, 0.25, 2.5);
    }
    for resistance in [-1.0, 0.0, 100.0, f64::MAX] {
        invalid(source.set_resistance(Channel::A, Ohms::new(resistance).unwrap()));
        assert_sample(&mut source, Channel::A, 0.25, 2.5);
    }
    assert!(matches!(
        source.configure(Channel::A, config(), deadline()),
        Err(InstrumentError::State(_))
    ));
    assert_eq!(
        source.output_state(Channel::A, deadline()).unwrap(),
        OutputState::On
    );
    assert_sample(&mut source, Channel::A, 0.25, 2.5);

    source.output_off(Channel::A, deadline()).unwrap();
    let mut voltage_limited = config();
    voltage_limited.limit_v = Volts::new(3.0).unwrap();
    voltage_limited.limit_p = Watts::new(20.0).unwrap();
    source
        .configure(Channel::A, voltage_limited, deadline())
        .unwrap();
    source
        .set_current(Channel::A, Amperes::new(0.25).unwrap(), deadline())
        .unwrap();
    source.output_on(Channel::A, deadline()).unwrap();
    invalid(source.set_current(Channel::A, Amperes::new(0.4).unwrap(), deadline()));
    assert_sample(&mut source, Channel::A, 0.25, 2.5);
}

#[test]
fn configuration_and_arithmetic_are_checked_before_mutation() {
    let mut source = new_source();
    let channel = Channel::A;
    source.configure(channel, config(), deadline()).unwrap();
    source
        .set_current(channel, Amperes::new(0.25).unwrap(), deadline())
        .unwrap();
    let base = config();
    let bad_configs = [
        ChannelConfig {
            limit_v: Volts::new(0.0).unwrap(),
            ..base
        },
        ChannelConfig {
            limit_i: Amperes::new(-1.0).unwrap(),
            ..base
        },
        ChannelConfig {
            limit_p: Watts::new(0.0).unwrap(),
            ..base
        },
        ChannelConfig {
            source_range_i: Amperes::new(0.5).unwrap(),
            ..base
        },
        ChannelConfig {
            measure_range_i: Amperes::new(0.5).unwrap(),
            ..base
        },
        ChannelConfig {
            measure_range_v: Volts::new(5.0).unwrap(),
            ..base
        },
        ChannelConfig { nplc: 0.0, ..base },
        ChannelConfig {
            nplc: f64::NAN,
            ..base
        },
        ChannelConfig {
            nplc: f64::INFINITY,
            ..base
        },
    ];
    for bad in bad_configs {
        invalid(source.configure(channel, bad, deadline()));
    }
    source.output_on(channel, deadline()).unwrap();
    assert_sample(&mut source, channel, 0.25, 2.5);

    source.output_off(channel, deadline()).unwrap();
    let huge = ChannelConfig {
        limit_v: Volts::new(f64::MAX).unwrap(),
        limit_i: Amperes::new(f64::MAX).unwrap(),
        limit_p: Watts::new(f64::MAX).unwrap(),
        source_range_i: Amperes::new(f64::MAX).unwrap(),
        measure_range_i: Amperes::new(f64::MAX).unwrap(),
        measure_range_v: Volts::new(f64::MAX).unwrap(),
        nplc: 1.0,
    };
    source.configure(channel, huge, deadline()).unwrap();
    source
        .set_current(channel, Amperes::new(0.25).unwrap(), deadline())
        .unwrap();
    source.output_on(channel, deadline()).unwrap();
    invalid(source.set_current(channel, Amperes::new(f64::MAX).unwrap(), deadline()));
    assert_sample(&mut source, channel, 0.25, 2.5);
    source
        .set_resistance(channel, Ohms::new(f64::MAX).unwrap())
        .unwrap();
    invalid(source.set_current(channel, Amperes::new(2.0).unwrap(), deadline()));
    let measured = source.measure(channel, deadline()).unwrap();
    assert_eq!(measured.current_a.get(), 0.25);
    assert!(measured.voltage_v.get().is_finite());
    for load in [0.0, -1.0] {
        invalid(MaishengSim::new_test_only([Ohms::new(load).unwrap(); 2]));
    }
}

#[test]
fn expired_trait_calls_do_not_change_configuration_or_output() {
    let mut source = new_source();
    source.configure(Channel::A, config(), deadline()).unwrap();
    source
        .set_current(Channel::A, Amperes::new(0.25).unwrap(), deadline())
        .unwrap();
    source.output_on(Channel::A, deadline()).unwrap();
    let past = Instant::now();
    let mut other = config();
    other.nplc = 2.0;
    timeout(source.configure(Channel::A, other, past));
    timeout(source.set_current(Channel::A, Amperes::new(0.1).unwrap(), past));
    timeout(source.output_on(Channel::A, past));
    timeout(source.output_off(Channel::A, past));
    timeout(source.measure(Channel::A, past));
    timeout(source.output_state(Channel::A, past));
    timeout(source.identity(past));
    timeout(source.errors(past));
    assert_eq!(
        source.output_state(Channel::A, deadline()).unwrap(),
        OutputState::On
    );
    assert_sample(&mut source, Channel::A, 0.25, 2.5);
    source.output_off(Channel::A, deadline()).unwrap();
    timeout(source.output_on(Channel::A, Instant::now()));
    assert_eq!(
        source.output_state(Channel::A, deadline()).unwrap(),
        OutputState::Off
    );
    assert_sample(&mut source, Channel::A, 0.0, 0.0);
}

#[test]
fn reference_profile_and_reserved_flags_cannot_enable_hardware() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let reference =
        fs::read_to_string(root.join("docs/protocols/maisheng_mp3001cr2.yaml")).unwrap();
    let profile: yaml_serde::Value = yaml_serde::from_str(&reference).unwrap();
    assert_eq!(profile["manufacturer"].as_str(), Some("Maisheng"));
    assert_eq!(profile["model"].as_str(), Some("MP3001CR2"));
    assert_eq!(profile["status"].as_str(), Some("unverified"));
    assert_eq!(profile["protocol_verified"].as_bool(), Some(false));
    for field in [
        "transport",
        "address",
        "framing",
        "commands",
        "registers",
        "limits",
        "output_behavior",
        "disconnect_behavior",
    ] {
        assert_eq!(profile[field].as_str(), Some("unverified"), "{field}");
    }
    assert!(Config::from_yaml(&reference).is_err());

    let shipped = fs::read_to_string(root.join("config/simulator.yaml")).unwrap();
    let mut flags: yaml_serde::Value = yaml_serde::from_str(&shipped).unwrap();
    assert_eq!(flags["hardware_enabled"].as_bool(), Some(false));
    for branch in ["maisheng_1", "maisheng_2"] {
        assert_eq!(
            flags["sources"][branch]["protocol_verified"].as_bool(),
            Some(false)
        );
        assert_eq!(
            flags["sources"][branch]["enabled_for_run"].as_bool(),
            Some(false)
        );
        flags["sources"][branch]["protocol_verified"] = yaml_serde::Value::Bool(true);
        flags["sources"][branch]["enabled_for_run"] = yaml_serde::Value::Bool(true);
    }
    flags["run_permission"] = yaml_serde::Value::String("approved".into());
    assert!(Config::from_yaml(&yaml_serde::to_string(&flags).unwrap()).is_ok());
    flags["hardware_enabled"] = yaml_serde::Value::Bool(true);
    let error = Config::from_yaml(&yaml_serde::to_string(&flags).unwrap()).unwrap_err();
    assert!(error.to_string().contains("Доступ к оборудованию запрещён"));
}
