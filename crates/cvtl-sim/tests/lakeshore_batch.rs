//! Regression: the 218 driver reads a batch frame from the simulator.

use cvtl_instruments::{
    lakeshore::{Channel, LakeShore218, SensorUnit},
    LoopbackTcp,
};
use cvtl_sim::lakeshore::LakeShoreSim;
use std::time::{Duration, Instant};

#[test]
fn batch_read_matches_simulated_channels() {
    let sim = LakeShoreSim::spawn().unwrap();
    let transport = LoopbackTcp::connect(sim.addr(), Duration::from_secs(2)).unwrap();
    let mut driver = LakeShore218::new(transport);
    let channels = [
        Channel::new(1).unwrap(),
        Channel::new(2).unwrap(),
        Channel::new(3).unwrap(),
        Channel::new(4).unwrap(),
    ];
    let deadline = Instant::now() + Duration::from_secs(2);
    let readings = driver
        .read_channels(&channels, SensorUnit::Volts, deadline)
        .unwrap();
    assert_eq!(readings.len(), 4);
    for reading in readings {
        let reading = reading.expect("healthy simulated input");
        assert_eq!(reading.kelvin.get(), 42.0);
    }
}
