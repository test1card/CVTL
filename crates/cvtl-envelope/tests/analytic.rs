use cvtl_core::units::{Amperes, Ohms, Watts};
use cvtl_envelope::{
    advise, advise_pair, check_current, ChannelSpec, LimitingFactor, Margins, ResistanceRange,
};

fn ohm(x: f64) -> Ohms {
    Ohms::new(x).unwrap()
}
fn amp(x: f64) -> Amperes {
    Amperes::new(x).unwrap()
}
fn watts(x: f64) -> Watts {
    Watts::new(x).unwrap()
}
fn spec(lo: f64, hi: f64, wire: f64, cap: Option<f64>) -> ChannelSpec {
    ChannelSpec {
        resistance: ResistanceRange::new(ohm(lo), ohm(hi)).unwrap(),
        loop_resistance: ohm(wire),
        heater_limit: cap.map(watts),
    }
}
fn close(a: f64, b: f64, tol: f64) {
    assert!((a - b).abs() <= tol, "{a} != {b}");
}

#[test]
fn section_7_table_with_and_without_margin() {
    for (r, nominal, margin) in [
        (25., 25., 20.3),
        (40., 40., 32.4),
        (50., 32., 25.9),
        (75., 21.3, 17.3),
        (150., 10.7, 8.6),
    ] {
        for (k, expected) in [(1., nominal), (0.9, margin)] {
            let p = advise(spec(r, r, 0., None), Margins::new(k, k).unwrap()).unwrap();
            close(p.source.power.get(), expected, 0.1);
            assert!(p.capped.is_none());
        }
    }
}

#[test]
fn actual_force_and_union_of_regimes() {
    let m = Margins::new(1., 1.).unwrap();
    assert!(!check_current(ohm(40. / 3.), ohm(0.), amp(3.), m).unwrap());
    assert!(check_current(ohm(2.), ohm(0.), amp(3.), m).unwrap());
    assert!(!check_current(ohm(2.), ohm(1.), amp(3.), m).unwrap());
    let p = advise(spec(40., 40., 10., None), m).unwrap().source;
    close(p.current.get(), 0.8, 1e-12);
    close(p.force_voltage.get(), 40., 1e-12);
    close(p.sense_voltage.get(), 32., 1e-12);
    close(p.power.get(), 25.6, 1e-12);
}

#[test]
fn independent_margins_headroom_and_ties() {
    let p = advise(spec(40., 40., 0., None), Margins::new(0.8, 0.9).unwrap())
        .unwrap()
        .source;
    close(p.power.get(), 25.6, 1e-12);
    close(p.current.get(), 0.8, 1e-12);
    close(p.voltage_headroom.get(), 0., 1e-12);
    let tie = advise(spec(40., 40., 0., None), Margins::new(1., 1.).unwrap())
        .unwrap()
        .source;
    assert_eq!(tie.limiting, LimitingFactor::CurrentAndVoltage);
}

#[test]
fn conservative_interval_includes_interior_crossover() {
    // The voltage-limited high-current regime crosses the current-limited
    // low-current regime at R=6 Ω.
    // With no wire, the lower envelope over [3, 10] occurs at the interior
    // switch R=6: both regimes provide 6 W.
    let p = advise(spec(3., 10., 0., None), Margins::new(1., 1.).unwrap())
        .unwrap()
        .source;
    close(p.power.get(), 6., 1e-12);
}

#[test]
fn heater_and_total_caps_are_distinct_from_source_capacity() {
    let m = Margins::new(1., 1.).unwrap();
    let p = advise_pair(
        spec(40., 40., 0., Some(10.)),
        spec(40., 40., 0., Some(20.)),
        m,
        Some(watts(25.)),
    )
    .unwrap();
    close(p.source_sum.get(), 80., 1e-12);
    close(p.a.capped.unwrap().power.get(), 10., 1e-12);
    close(p.b.capped.unwrap().power.get(), 20., 1e-12);
    close(p.capped_sum.unwrap().get(), 25., 1e-12);
    assert!(advise_pair(
        spec(40., 40., 0., None),
        spec(40., 40., 0., Some(20.)),
        m,
        Some(watts(25.))
    )
    .unwrap()
    .capped_sum
    .is_none());
}

#[test]
fn invalid_inputs_rejected() {
    for k in [0., -1., 1.01, f64::NAN, f64::INFINITY] {
        assert!(Margins::new(k, 1.).is_err());
        assert!(Margins::new(1., k).is_err());
    }
    assert!(ResistanceRange::new(ohm(0.), ohm(1.)).is_err());
    assert!(ResistanceRange::new(ohm(2.), ohm(1.)).is_err());
    let m = Margins::new(1., 1.).unwrap();
    assert!(advise(spec(1., 1., -1., None), m).is_err());
    assert!(advise(spec(1., 1., 0., Some(-1.)), m).is_err());
    assert!(check_current(ohm(1.), ohm(0.), amp(-1.), m).is_err());
    assert!(advise_pair(
        spec(1., 1., 0., Some(1.)),
        spec(1., 1., 0., Some(1.)),
        m,
        Some(watts(0.))
    )
    .is_err());
}

#[test]
fn rounded_points_obey_strict_source_and_heater_limits() {
    let m = Margins::new(1., 1.).unwrap();
    let voltage_boundary = advise(spec(64.594, 64.594, 0., None), m).unwrap().source;
    assert!(check_current(
        voltage_boundary.resistance,
        ohm(0.),
        voltage_boundary.current,
        m
    )
    .unwrap());
    let capped = advise(spec(0.108, 0.108, 0., Some(0.1)), m)
        .unwrap()
        .capped
        .unwrap();
    assert!(check_current(capped.resistance, ohm(0.), capped.current, m).unwrap());
    assert!(capped.power.get() <= 0.1);
}
