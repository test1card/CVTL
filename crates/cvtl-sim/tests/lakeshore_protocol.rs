use cvtl_instruments::{
    lakeshore::{
        parse_reading, parse_status, Channel, LakeShore218, SensorReading, SensorUnit, Thermometer,
    },
    LoopbackTcp,
};
use cvtl_sim::lakeshore::{Fault, LakeShoreSim};
use std::time::{Duration, Instant};

fn driver(sim: &LakeShoreSim) -> LakeShore218<LoopbackTcp> {
    LakeShore218::new(LoopbackTcp::connect(sim.addr(), Duration::from_millis(300)).unwrap())
}

#[test]
fn reads_temperature_and_sensor_units_with_timestamps_and_repeated_values() {
    let sim = LakeShoreSim::spawn().unwrap();
    let mut d = driver(&sim);
    let start = Instant::now();
    let a = d
        .read(
            Channel::new(1).unwrap(),
            SensorUnit::Volts,
            start + Duration::from_secs(2),
        )
        .unwrap();
    let b = d
        .read(
            Channel::new(1).unwrap(),
            SensorUnit::Volts,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(a.kelvin, b.kelvin);
    assert_eq!(a.sensor, b.sensor);
    assert!(matches!(a.sensor, SensorReading::Volts(_)));
    assert!(
        start <= a.started
            && a.started <= a.finished
            && a.finished <= b.started
            && b.started <= b.finished
    );
    assert!(a.measurement_time.is_none());
}

#[test]
fn validates_channels_status_and_numeric_replies() {
    assert!(Channel::new(0).is_err());
    assert!(Channel::new(9).is_err());
    assert!(Channel::new(8).is_ok());
    for data in [
        b"NaN".as_slice(),
        b"inf",
        b"+1.0,2.0",
        b"1.0junk",
        b"",
        b"\xff",
    ] {
        assert!(parse_reading(data).is_err());
    }
    for data in [b"-1".as_slice(), b"256", b"1.0", b"junk"] {
        assert!(parse_status(data).is_err());
    }
    assert!(parse_status(b"0").is_ok());
}

#[test]
fn nonzero_range_status_and_malformed_values_fail_closed() {
    for fault in [
        Fault::Status(16),
        Fault::Status(32),
        Fault::Status(64),
        Fault::Status(128),
        Fault::MalformedReading,
    ] {
        let sim = LakeShoreSim::spawn().unwrap();
        sim.set_fault(fault);
        let mut d = driver(&sim);
        assert!(d
            .read(
                Channel::new(8).unwrap(),
                SensorUnit::Ohms,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err());
    }
}

#[test]
fn disconnect_requires_explicit_recovery_with_fresh_transport() {
    let sim = LakeShoreSim::spawn().unwrap();
    sim.set_fault(Fault::Disconnect);
    let mut d = driver(&sim);
    assert!(d
        .read(
            Channel::new(1).unwrap(),
            SensorUnit::Ohms,
            Instant::now() + Duration::from_secs(1)
        )
        .is_err());
    assert!(!d.is_usable());
    sim.set_fault(Fault::None);
    d.recover(LoopbackTcp::connect(sim.addr(), Duration::from_millis(300)).unwrap());
    assert!(d
        .read(
            Channel::new(1).unwrap(),
            SensorUnit::Ohms,
            Instant::now() + Duration::from_secs(1)
        )
        .is_ok());
}

#[test]
fn resistance_unit_and_timeout_are_explicit() {
    let sim = LakeShoreSim::spawn().unwrap();
    let mut d = driver(&sim);
    let r = d
        .read(
            Channel::new(2).unwrap(),
            SensorUnit::Ohms,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    assert!(matches!(r.sensor, SensorReading::Ohms(_)));
    sim.set_fault(Fault::Silence);
    assert!(d
        .read(
            Channel::new(2).unwrap(),
            SensorUnit::Ohms,
            Instant::now() + Duration::from_millis(100)
        )
        .is_err());
    assert!(!d.is_usable());
}

#[test]
fn final_status_fault_rejects_numeric_readings_and_latches_transport() {
    use cvtl_instruments::{InstrumentError, MessageTransport, Result};
    use std::sync::{Arc, Mutex};
    struct Script {
        calls: Arc<Mutex<Vec<String>>>,
        usable: bool,
    }
    impl MessageTransport for Script {
        fn send(&mut self, _: &str, _: Instant) -> Result<()> {
            unreachable!()
        }
        fn query(&mut self, command: &str, _: Instant) -> Result<Vec<u8>> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(command.to_owned());
            Ok(match calls.len() {
                1 => b"000,000,000,000,000,000,000,000".to_vec(),
                2 => b"+42.000,+42.000,+42.000,+42.000,+42.000,+42.000,+42.000,+42.000".to_vec(),
                3 => b"+1.234,+1.234,+1.234,+1.234,+1.234,+1.234,+1.234,+1.234".to_vec(),
                4 => b"000,000,032,000,000,000,000,000".to_vec(),
                _ => panic!("unexpected query"),
            })
        }
        fn invalidate(&mut self) {
            self.usable = false;
        }
        fn is_usable(&self) -> bool {
            self.usable
        }
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut d = LakeShore218::new(Script {
        calls: calls.clone(),
        usable: true,
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(d
        .read(Channel::new(3).unwrap(), SensorUnit::Volts, deadline)
        .is_err());
    assert!(matches!(
        d.read(Channel::new(3).unwrap(), SensorUnit::Volts, deadline),
        Err(InstrumentError::Disconnected)
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        ["RDGST?", "KRDG?", "SRDG?", "RDGST?"]
    );
}
