//! Analytical, test-only plant scenarios; all times are virtual 100 ms ticks.
use cvtl_core::units::{Amperes, Ohms, Watts};
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
use cvtl_sim::power_loop::{Branch, Plant, PlantConfig};

fn plant(command_delay_ticks: usize, measurement_delay_ticks: usize) -> Plant {
    Plant::new(PlantConfig {
        branches: [
            Branch {
                resistance_ohm: r(10.0),
                current_step_a: a(0.001),
            },
            Branch {
                resistance_ohm: r(20.0),
                current_step_a: a(0.001),
            },
        ],
        command_delay_ticks,
        measurement_delay_ticks,
        noise_amplitude_w: w(0.0),
        noise_seed: 7,
    })
    .unwrap()
}

#[test]
fn analytic_power_and_current_quantization() {
    let mut p = plant(0, 0);
    let sample = p.tick([a(0.1004), a(0.2004)]).unwrap();
    assert!((sample.actual_power_w[0].get() - 0.1).abs() < 1e-12);
    assert!((sample.actual_power_w[1].get() - 0.8).abs() < 1e-12);
    assert_eq!(sample.measured_power_w, sample.actual_power_w);
}

#[test]
fn independent_command_and_measurement_delays_are_fifo() {
    let mut p = plant(1, 1);
    let first = p.tick([a(0.1), a(0.0)]).unwrap();
    let second = p.tick([a(0.2), a(0.0)]).unwrap();
    let third = p.tick([a(0.0), a(0.0)]).unwrap();
    assert_eq!(first.measurement_tick, None);
    assert_eq!(second.measurement_tick, Some(0));
    assert_eq!(third.measurement_tick, Some(1));
    assert_eq!(first.actual_power_w, [w(0.0), w(0.0)]);
    assert_eq!(first.measured_power_w, [w(0.0), w(0.0)]);
    assert!((second.actual_power_w[0].get() - 0.1).abs() < 1e-12);
    assert_eq!(second.measured_power_w, [w(0.0), w(0.0)]);
    assert!((third.actual_power_w[0].get() - 0.4).abs() < 1e-12);
    assert!((third.measured_power_w[0].get() - 0.1).abs() < 1e-12);
}

#[test]
fn resistance_drift_and_branch_fault_affect_actual_power() {
    let mut p = plant(0, 0);
    p.set_resistance(0, r(11.0)).unwrap();
    assert!((p.tick([a(0.1), a(0.1)]).unwrap().actual_power_w[0].get() - 0.11).abs() < 1e-12);
    p.set_failed(0, true).unwrap();
    assert_eq!(
        p.tick([a(0.1), a(0.1)]).unwrap().actual_power_w[0].get(),
        0.0
    );
}

#[test]
fn seeded_noise_replays_and_stays_bounded() {
    let config = PlantConfig {
        branches: [Branch {
            resistance_ohm: r(10.0),
            current_step_a: a(0.001),
        }; 2],
        command_delay_ticks: 0,
        measurement_delay_ticks: 0,
        noise_amplitude_w: w(0.002),
        noise_seed: 42,
    };
    let mut plant_a = Plant::new(config).unwrap();
    let mut b = Plant::new(config).unwrap();
    for _ in 0..100 {
        let x = plant_a.tick([a(0.2), a(0.2)]).unwrap();
        let y = b.tick([a(0.2), a(0.2)]).unwrap();
        assert_eq!(x.measured_power_w, y.measured_power_w);
        for branch in 0..2 {
            assert!(
                (x.measured_power_w[branch].get() - x.actual_power_w[branch].get()).abs() <= 0.002
            );
        }
    }
}

#[test]
fn delay_allocations_are_bounded() {
    let config = PlantConfig {
        branches: [Branch {
            resistance_ohm: r(10.0),
            current_step_a: a(0.001),
        }; 2],
        command_delay_ticks: usize::MAX,
        measurement_delay_ticks: 0,
        noise_amplitude_w: w(0.0),
        noise_seed: 1,
    };
    assert!(Plant::new(config).is_err());
    assert!(Plant::new(PlantConfig {
        command_delay_ticks: 0,
        measurement_delay_ticks: usize::MAX,
        ..config
    })
    .is_err());
}

#[test]
fn negative_command_and_overflow_return_error() {
    let mut p = plant(0, 0);
    assert!(p.tick([a(-1.0), a(0.0)]).is_err());
    assert!(p.tick([a(f64::MAX), a(0.0)]).is_err());
    assert_eq!(p.tick([a(0.0); 2]).unwrap().measurement_tick, Some(0));
}
