//! Reproducible closed-loop scenario, 10 Hz virtual time. No OS scheduling or hardware.
use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    *,
};
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_sim::power_loop::{Branch, Plant, PlantConfig};

fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn rate(x: f64) -> WattsPerSecond {
    WattsPerSecond::new(x).unwrap()
}

fn fixture() -> (Controller, Plant) {
    let branch = ChannelSpec {
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
        current_quantum: Some(a(0.0001)),
        derivative: None,
    };
    let cfg = ControlConfig {
        pi: [pi; 2],
        branches: [branch; 2],
        margins: Margins::new(1.0, 1.0).unwrap(),
        total_limit: w(6.0),
        max_sample_age: s(0.5),
        temperature_hold: None,
    };
    let allocator = Allocator::new(
        None,
        rate(100.0),
        rate(100.0),
        [rate(100.0); 2],
        [rate(100.0); 2],
    )
    .unwrap();
    let plant = Plant::new(PlantConfig {
        branches: [Branch {
            resistance_ohm: r(10.0),
            current_step_a: a(0.0001),
        }; 2],
        command_delay_ticks: 0,
        measurement_delay_ticks: 0,
        noise_amplitude_w: w(0.0),
        noise_seed: 42,
    })
    .unwrap();
    (Controller::new(cfg, allocator).unwrap(), plant)
}

#[test]
fn reachable_step_settles() {
    let (mut c, mut plant) = fixture();
    let mut sample = plant.tick([a(0.0), a(0.0)]).unwrap();
    let mut powers = Vec::new();
    for tick in 0..120 {
        let measurements = std::array::from_fn(|i| {
            let current = sample.measured_current_a[i].get();
            let voltage = if current == 0.0 {
                0.0
            } else {
                sample.measured_power_w[i].get() / current
            };
            Some(TimedPowerSample {
                at: s(sample
                    .measurement_tick
                    .map_or(0.0, |t| t.saturating_sub(1) as f64 * 0.1)),
                sample: PowerSample {
                    voltage: Volts::new(voltage).unwrap(),
                    current: a(current),
                },
            })
        });
        let output = c
            .tick(Tick {
                demand: Demand::Power(w(if tick < 60 { 1.0 } else { 2.0 })),
                permit: HeatPermit::Limit(w(6.0)),
                branches_available: [true; 2],
                measurements,
                now: s(tick as f64 * 0.1),
                dt: s(0.1),
            })
            .unwrap();
        let currents = match output.action {
            Actuation::Drive(i) => i.map(|x| x.get()),
            Actuation::Off => [0.0; 2],
        };
        sample = plant.tick(currents.map(a)).unwrap();
        powers.push(sample.actual_power_w.iter().map(|x| x.get()).sum::<f64>());
    }
    let settling_tick = (0..20)
        .find(|&start| powers[start..60].iter().all(|p| (p - 1.0).abs() <= 0.02))
        .expect("does not settle to ±2% in 2 s");
    let steady_error = powers[40..60]
        .iter()
        .map(|p| (p - 1.0).abs())
        .fold(0.0, f64::max);
    assert!(steady_error <= 0.01);
    println!("{{\"scenario\":\"1W_step\",\"settling_s\":{},\"steady_max_error_w\":{},\"virtual_tick_s\":0.1}}", (settling_tick + 1) as f64 * 0.1, steady_error);
    let second = &powers[60..];
    let second_tick = (0..20)
        .find(|&start| second[start..].iter().all(|p| (p - 2.0).abs() <= 0.04))
        .expect("2 W step does not settle in 2 s");
    let second_error = second[40..]
        .iter()
        .map(|p| (p - 2.0).abs())
        .fold(0.0, f64::max);
    assert!(second_error <= 0.02);
    println!("{{\"scenario\":\"1_to_2W_step\",\"settling_s\":{},\"steady_max_error_w\":{},\"virtual_tick_s\":0.1}}", (second_tick + 1) as f64 * 0.1, second_error);
}

#[test]
fn drift_noise_latency_saturation_and_failure() {
    let (mut c, _) = fixture();
    let mut plant = Plant::new(PlantConfig {
        branches: [Branch {
            resistance_ohm: r(10.0),
            current_step_a: a(0.0001),
        }; 2],
        command_delay_ticks: 1,
        measurement_delay_ticks: 1,
        noise_amplitude_w: w(0.001),
        noise_seed: 42,
    })
    .unwrap();
    let mut sample = plant.tick([a(0.0), a(0.0)]).unwrap();
    let mut powers = Vec::new();
    for tick in 0..100 {
        if tick == 30 {
            plant.set_resistance(0, r(11.0)).unwrap();
            plant.set_resistance(1, r(11.0)).unwrap();
        }
        let measurements = std::array::from_fn(|i| {
            let current = sample.measured_current_a[i].get();
            let voltage = if current == 0.0 {
                0.0
            } else {
                sample.measured_power_w[i].get() / current
            };
            Some(TimedPowerSample {
                // Tick zero primes the plant before the controller starts.
                at: s(sample
                    .measurement_tick
                    .map_or(0.0, |t| t.saturating_sub(1) as f64 * 0.1)),
                sample: PowerSample {
                    voltage: Volts::new(voltage).unwrap(),
                    current: a(current),
                },
            })
        });
        let output = c
            .tick(Tick {
                demand: Demand::Power(w(1.0)),
                permit: HeatPermit::Limit(w(6.0)),
                branches_available: [true; 2],
                measurements,
                now: s(tick as f64 * 0.1),
                dt: s(0.1),
            })
            .unwrap();
        let currents = match output.action {
            Actuation::Drive(i) => i.map(|x| x.get()),
            Actuation::Off => [0.0; 2],
        };
        sample = plant.tick(currents.map(a)).unwrap();
        powers.push(sample.actual_power_w.iter().map(|x| x.get()).sum::<f64>());
    }
    let error = powers[80..]
        .iter()
        .map(|p| (p - 1.0).abs())
        .fold(0.0, f64::max);
    assert!(error <= 0.01, "drift/noise error {error}");
    println!("{{\"scenario\":\"drift_noise_latency\",\"steady_max_error_w\":{error},\"command_delay_s\":0.1,\"measurement_delay_s\":0.1,\"prior_tick_handoff_s\":0.1,\"noise_bound_per_branch_w\":0.001}}");
    let measurements = std::array::from_fn(|i| {
        let current = sample.measured_current_a[i].get();
        Some(TimedPowerSample {
            at: s(sample
                .measurement_tick
                .map_or(0.0, |t| t.saturating_sub(1) as f64 * 0.1)),
            sample: PowerSample {
                voltage: Volts::new(sample.measured_power_w[i].get() / current).unwrap(),
                current: a(current),
            },
        })
    });
    let saturated = c
        .tick(Tick {
            demand: Demand::Power(w(20.0)),
            permit: HeatPermit::Limit(w(2.0)),
            branches_available: [true; 2],
            measurements,
            now: s(10.0),
            dt: s(0.1),
        })
        .unwrap();
    assert!(saturated.limited.get() <= 2.0);
    let failed = c
        .tick(Tick {
            demand: Demand::Power(w(1.0)),
            permit: HeatPermit::Limit(w(6.0)),
            branches_available: [false, true],
            measurements,
            now: s(10.1),
            dt: s(0.1),
        })
        .unwrap();
    assert!(matches!(failed.action, Actuation::Off));
}
