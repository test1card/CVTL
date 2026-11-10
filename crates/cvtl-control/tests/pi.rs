use cvtl_control::pi::{Pi, PiConfig, PowerSample};
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts};

fn config() -> PiConfig {
    PiConfig {
        kp: 0.5,
        ki: PerSecond::new(1.0).unwrap(),
        integral_limit: Watts::new(5.0).unwrap(),
        resistance_initial: Ohms::new(10.0).unwrap(),
        resistance_min: Ohms::new(5.0).unwrap(),
        resistance_max: Ohms::new(20.0).unwrap(),
        resistance_tau: Seconds::new(1.0).unwrap(),
        min_current: Amperes::new(0.01).unwrap(),
        current_quantum: None,
        derivative: None,
    }
}
fn sample(v: f64, i: f64) -> Option<PowerSample> {
    Some(PowerSample {
        voltage: Volts::new(v).unwrap(),
        current: Amperes::new(i).unwrap(),
    })
}
fn step(
    pi: &mut Pi,
    p: f64,
    m: Option<PowerSample>,
    dt: f64,
    max: f64,
) -> cvtl_control::pi::PiOutput {
    pi.step(
        Watts::new(p).unwrap(),
        m,
        Seconds::new(dt).unwrap(),
        Amperes::new(max).unwrap(),
    )
    .unwrap()
}
#[test]
fn feedforward_and_measured_power_use_sense_voltage() {
    let mut c = config();
    c.kp = 0.0;
    c.ki = PerSecond::new(0.0).unwrap();
    let out = step(&mut Pi::new(c).unwrap(), 10.0, sample(10.0, 1.0), 0.1, 3.0);
    assert!((out.current.get() - 1.0).abs() < 1e-12);
    assert_eq!(out.measured_power.unwrap().get(), 10.0);
}
#[test]
fn resistance_filters_freezes_and_is_bounded() {
    let mut pi = Pi::new(config()).unwrap();
    let first = step(&mut pi, 1.0, sample(100.0, 1.0), 1.0, 2.0);
    assert!(first.resistance.get() > 10.0 && first.resistance.get() <= 20.0);
    let next = step(&mut pi, 1.0, sample(1.0, 0.001), 1.0, 2.0);
    assert_eq!(first.resistance, next.resistance);
}
#[test]
fn final_ceiling_and_downward_quantization_are_authoritative() {
    let mut c = config();
    c.current_quantum = Some(Amperes::new(0.1).unwrap());
    let out = step(&mut Pi::new(c).unwrap(), 100.0, sample(0.0, 0.0), 0.1, 0.35);
    assert!(out.saturated && out.current.get() <= 0.35);
    assert!((out.current.get() - 0.3).abs() < 1e-12);
}
#[test]
fn invalid_inputs_are_errors_and_do_not_create_commands() {
    let mut pi = Pi::new(config()).unwrap();
    assert!(pi
        .step(
            Watts::new(1.0).unwrap(),
            None,
            Seconds::new(1.0).unwrap(),
            Amperes::new(1.0).unwrap()
        )
        .is_err());
    assert!(pi
        .step(
            Watts::new(-1.0).unwrap(),
            sample(1.0, 1.0),
            Seconds::new(1.0).unwrap(),
            Amperes::new(1.0).unwrap()
        )
        .is_err());
    assert!(pi
        .step(
            Watts::new(1.0).unwrap(),
            sample(-1.0, 1.0),
            Seconds::new(1.0).unwrap(),
            Amperes::new(1.0).unwrap()
        )
        .is_err());
    assert!(Pi::new(PiConfig {
        resistance_min: Ohms::new(0.0).unwrap(),
        ..config()
    })
    .is_err());
}
#[test]
fn rebase_matches_actual_output_when_unsaturated() {
    let mut pi = Pi::new(config()).unwrap();
    pi.rebase(
        Watts::new(5.0).unwrap(),
        sample(5.0, 0.5),
        Amperes::new(0.6).unwrap(),
    )
    .unwrap();
    let out = step(&mut pi, 5.0, sample(5.0, 0.5), 0.000000001, 3.0);
    assert!((out.current.get() - 0.6).abs() < 1e-6);
}
#[test]
fn sustained_saturation_does_not_wind_up() {
    let mut pi = Pi::new(config()).unwrap();
    for _ in 0..500 {
        step(&mut pi, 50.0, sample(0.0, 0.0), 0.1, 0.1);
    }
    let out = step(&mut pi, 0.0, sample(0.0, 0.0), 0.1, 10.0);
    assert!(out.current.get() < 1e-9);
}
#[test]
fn rebase_clamps_integral_and_retains_hard_ceiling() {
    let mut c = config();
    c.integral_limit = Watts::new(0.001).unwrap();
    let mut pi = Pi::new(c).unwrap();
    pi.rebase(
        Watts::new(100.0).unwrap(),
        sample(0.0, 0.0),
        Amperes::new(0.0).unwrap(),
    )
    .unwrap();
    let out = step(&mut pi, 100.0, sample(0.0, 0.0), 0.1, 0.35);
    assert!(out.current.get() <= 0.35);
}
#[test]
fn quantization_never_returns_an_off_grid_ceiling() {
    let mut c = config();
    c.current_quantum = Some(Amperes::new(0.1).unwrap());
    let cap = f64::from_bits((0.3_f64).to_bits() - 1);
    let out = step(&mut Pi::new(c).unwrap(), 100.0, sample(0.0, 0.0), 0.1, cap);
    assert!(out.current.get() <= cap);
    assert_eq!(out.current.get(), 0.2);
}
#[test]
fn finite_high_demand_with_tiny_resistance_saturates_without_overflow() {
    let mut c = config();
    c.kp = 0.0;
    c.ki = PerSecond::new(0.0).unwrap();
    c.resistance_min = Ohms::new(1e-300).unwrap();
    c.resistance_initial = c.resistance_min;
    c.resistance_max = c.resistance_min;
    let mut pi = Pi::new(c).unwrap();
    let out = step(&mut pi, 1e100, sample(0.0, 0.0), 0.1, 0.35);
    assert!(out.saturated);
    assert!(out.current.get() <= 0.35);
}
