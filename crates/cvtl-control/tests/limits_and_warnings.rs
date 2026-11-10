use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    Actuation, ControlConfig, Controller, Demand, Equivalence, Fault, HeatPermit, Tick,
    TimedPowerSample, Warning,
};
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn a(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn r(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn controller() -> Controller {
    let branch = ChannelSpec {
        resistance: ResistanceRange::new(r(10.), r(100.)).unwrap(),
        loop_resistance: r(0.),
        heater_limit: Some(w(10.)),
    };
    let pi = PiConfig {
        kp: 0.3,
        ki: PerSecond::new(1.).unwrap(),
        integral_limit: w(40.),
        resistance_initial: r(10.),
        resistance_min: r(10.),
        resistance_max: r(100.),
        resistance_tau: s(1.),
        min_current: a(0.001),
        current_quantum: None,
        derivative: None,
    };
    let cfg = ControlConfig {
        pi: [pi; 2],
        branches: [branch; 2],
        margins: Margins::new(0.9, 0.9).unwrap(),
        total_limit: w(10.),
        max_sample_age: s(0.05),
        temperature_hold: None,
    };
    let rate = WattsPerSecond::new(100.).unwrap();
    Controller::new(
        cfg,
        Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap(),
    )
    .unwrap()
}
fn tick(now: f64, at: f64, demand: f64, cap: f64, currents: [Amperes; 2]) -> Tick {
    Tick {
        demand: Demand::Power(w(demand)),
        permit: HeatPermit::Limit(w(cap)),
        branches_available: [true; 2],
        measurements: currents.map(|i| {
            Some(TimedPowerSample {
                at: s(at),
                sample: PowerSample {
                    current: i,
                    voltage: Volts::new(i.get() * 90.).unwrap(),
                },
            })
        }),
        now: s(now),
        dt: s(0.1),
    }
}
fn currents(action: Actuation) -> [Amperes; 2] {
    match action {
        Actuation::Drive(i) => i,
        Actuation::Off => [a(0.); 2],
    }
}
#[test]
fn ordinary_saturation_remains_safe_and_recovers_without_reset() {
    let mut c = controller();
    let mut previous = [a(0.); 2];
    for n in 0..8 {
        let out = c
            .tick(tick(n as f64 * 0.1, n as f64 * 0.1, 100., 0.5, previous))
            .unwrap();
        assert_eq!(out.fault, None);
        assert_eq!(out.equivalence, Equivalence::Limited);
        previous = currents(out.action);
        let worst: f64 = previous.iter().map(|i| i.get() * i.get() * 100.).sum();
        assert!(worst <= 0.5, "{worst}");
    }
    let out = c.tick(tick(0.8, 0.8, 0.1, 10., previous)).unwrap();
    assert_eq!(out.fault, None);
    assert!(matches!(out.action, Actuation::Drive(_)));
}
#[test]
fn stale_warning_holds_then_clears_and_repeated_stale_latches() {
    let mut c = controller();
    let first = c.tick(tick(0., 0., 2., 10., [a(0.); 2])).unwrap();
    let previous = currents(first.action);
    assert!(previous.iter().any(|i| i.get() > 0.));
    let stale = c.tick(tick(0.1, 0., 2., 10., previous)).unwrap();
    assert_eq!(stale.warning, Some(Warning::MeasurementStale));
    assert_eq!(stale.fault, None);
    assert_eq!(stale.measured, None);
    assert_eq!(stale.action, first.action);
    let fresh = c.tick(tick(0.2, 0.2, 2., 10., previous)).unwrap();
    assert_eq!(fresh.warning, None);
    assert_eq!(fresh.fault, None);
    let previous = currents(fresh.action);
    let stale = c.tick(tick(0.3, 0.2, 2., 10., previous)).unwrap();
    assert_eq!(stale.warning, Some(Warning::MeasurementStale));
    assert_eq!(stale.fault, None);
    let repeated = c.tick(tick(0.4, 0.2, 2., 10., previous)).unwrap();
    assert_eq!(repeated.fault, Some(Fault::MeasurementStale));
    assert_eq!(repeated.action, Actuation::Off);
}
#[test]
fn missing_sample_latches_immediately_and_reset_allows_fresh_tick() {
    let mut c = controller();
    c.tick(tick(0., 0., 2., 10., [a(0.); 2])).unwrap();
    let mut missing = tick(0.1, 0.1, 2., 10., [a(0.); 2]);
    missing.measurements[0] = None;
    let out = c.tick(missing).unwrap();
    assert_eq!(out.fault, Some(Fault::MeasurementUnavailable));
    assert_eq!(out.action, Actuation::Off);
    c.reset_fault();
    let out = c.tick(tick(0.2, 0.2, 2., 10., [a(0.); 2])).unwrap();
    assert_eq!(out.fault, None);
    assert!(matches!(out.action, Actuation::Drive(_)));
}
#[test]
fn stale_hold_respects_tighter_permit() {
    let mut c = controller();
    let first = c.tick(tick(0., 0., 8., 10., [a(0.); 2])).unwrap();
    let previous = currents(first.action);
    let out = c.tick(tick(0.1, 0., 8., 0.01, previous)).unwrap();
    assert_eq!(out.warning, Some(Warning::MeasurementStale));
    assert_eq!(out.fault, None);
    let held = currents(out.action);
    assert!(held.iter().zip(previous).all(|(new, old)| *new <= old));
    assert!(held.iter().map(|i| i.get() * i.get() * 100.).sum::<f64>() <= 0.01);
}

#[test]
fn stale_sample_cannot_hold_heat_after_nonpositive_request() {
    let mut c = controller();
    let first = c.tick(tick(0.0, 0.0, 2.0, 10.0, [a(0.0); 2])).unwrap();
    let previous = currents(first.action);
    assert!(previous.iter().any(|i| i.get() > 0.0));
    let out = c.tick(tick(0.1, 0.0, -1.0, 10.0, previous)).unwrap();
    assert_eq!(out.action, Actuation::Off);
}
