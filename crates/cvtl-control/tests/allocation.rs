use cvtl_control::allocation::Allocator;
use cvtl_core::units::{Seconds, Watts, WattsPerSecond};

fn w(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn s(x: f64) -> Seconds {
    Seconds::new(x).unwrap()
}
fn rate(x: f64) -> WattsPerSecond {
    WattsPerSecond::new(x).unwrap()
}
fn allocator(coarse: Option<usize>, up: f64, down: f64) -> Allocator {
    Allocator::new(coarse, rate(up), rate(down), [rate(up); 2], [rate(down); 2]).unwrap()
}
fn near(x: f64, y: f64) {
    assert!((x - y).abs() < 1e-9, "{x} != {y}");
}

#[test]
fn proportional_and_signed_request() {
    let mut a = allocator(None, 100., 100.);
    let x = a
        .step(w(6.), [w(2.), w(4.)], w(10.), [true; 2], s(1.))
        .unwrap();
    near(x.targets[0].get(), 2.);
    near(x.targets[1].get(), 4.);
    let x = a
        .step(w(-3.), [w(2.), w(4.)], w(10.), [true; 2], s(1.))
        .unwrap();
    near(x.requested.get(), -3.);
    near(x.limited.get(), 0.);
}

#[test]
fn coarse_branch_decreases_first() {
    let mut a = allocator(Some(0), 100., 100.);
    a.step(w(8.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    let x = a.step(w(5.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    near(x.targets[0].get(), 1.);
    near(x.targets[1].get(), 4.);
}

#[test]
fn branch_loss_shuts_both_immediately() {
    let mut a = allocator(None, 100., 0.01);
    a.step(w(8.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    let x = a
        .step(w(8.), [w(5.); 2], w(10.), [true, false], s(1.))
        .unwrap();
    near(x.targets[0].get(), 0.);
    near(x.targets[1].get(), 0.);
    assert!(x.branch_loss);
}

#[test]
fn hard_cap_overrides_downward_slew() {
    let mut a = allocator(None, 100., 0.01);
    a.step(w(8.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    let x = a
        .step(w(8.), [w(1.), w(5.)], w(3.), [true; 2], s(1.))
        .unwrap();
    assert!(x.targets[0].get() <= 1. && x.targets[0].get() + x.targets[1].get() <= 3. + 1e-12);
    assert!(x.hard_limit);
}

#[test]
fn invalid_input_preserves_state() {
    let mut a = allocator(None, 1., 1.);
    a.step(w(1.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    assert!(a
        .step(w(2.), [w(-1.), w(5.)], w(10.), [true; 2], s(1.))
        .is_err());
    let x = a.step(w(2.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    near(x.targets[0].get() + x.targets[1].get(), 2.);
}

#[test]
fn independent_total_and_branch_rates() {
    let mut a = Allocator::new(
        None,
        rate(2.),
        rate(1.),
        [rate(0.5), rate(4.)],
        [rate(0.25), rate(4.)],
    )
    .unwrap();
    let x = a
        .step(w(10.), [w(5.); 2], w(10.), [true; 2], s(1.))
        .unwrap();
    assert!(x.targets[0].get() <= 0.5 && x.limited.get() <= 2. + 1e-12);
    let old = x.limited.get();
    let old_branch = x.targets[0].get();
    let x = a.step(w(0.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    assert!(x.limited.get() >= old - 1. - 1e-12);
    assert!(x.targets[0].get() >= old_branch - 0.25 - 1e-12);
}

#[test]
fn reset_clears_slew_history() {
    let mut a = allocator(None, 1., 0.);
    a.step(w(2.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    a.reset();
    let x = a.step(w(0.), [w(5.); 2], w(10.), [true; 2], s(1.)).unwrap();
    near(x.limited.get(), 0.);
}
