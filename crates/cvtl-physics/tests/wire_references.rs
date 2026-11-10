use cvtl_core::units::{Amperes, Kelvin, Meters, OhmMeters, SquareMeters, WattsPerMeterKelvin};
use cvtl_materials::ThermalConductivityCurve;
use cvtl_physics::wire::{solve, ResistivityCurve, WireSegment};

fn k(value: f64, lo: f64, hi: f64) -> ThermalConductivityCurve {
    ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(lo).unwrap(),
            WattsPerMeterKelvin::new(value).unwrap(),
        ),
        (
            Kelvin::new(hi).unwrap(),
            WattsPerMeterKelvin::new(value).unwrap(),
        ),
    ])
    .unwrap()
}
fn segment() -> WireSegment {
    WireSegment {
        conductivity: k(2.0, 1.0, 100.0),
        resistivity: None,
        length_m: Meters::new(2.0).unwrap(),
        area_m2: SquareMeters::new(0.5).unwrap(),
        current_a: Amperes::new(0.0).unwrap(),
        start_k: Kelvin::new(10.0).unwrap(),
        end_k: Kelvin::new(20.0).unwrap(),
        radiation: None,
    }
}
fn near(a: f64, b: f64, tolerance: f64) {
    assert!((a - b).abs() <= tolerance, "{a} != {b}");
}

#[test]
fn constant_k_without_current() {
    let r = solve(&segment()).unwrap();
    near(r.heat_into_start_w.get(), 5.0, 1e-8);
    near(r.heat_into_end_w.get(), -5.0, 1e-8);
    near(r.joule_w.get(), 0.0, 1e-12);
}

#[test]
fn constant_resistivity_gives_parabolic_flux_and_conserves_power() {
    let mut s = segment();
    s.current_a = Amperes::new(2.0).unwrap();
    s.resistivity = Some(ResistivityCurve::constant(OhmMeters::new(0.25).unwrap()).unwrap());
    let r = solve(&s).unwrap();
    // I²ρL/A = 4 W; the conductive flux is 5 W.
    near(r.heat_into_start_w.get(), 7.0, 1e-7);
    near(r.heat_into_end_w.get(), -3.0, 1e-7);
    near(r.joule_w.get(), 4.0, 1e-7);
    near(
        r.heat_into_start_w.get() + r.heat_into_end_w.get(),
        r.joule_w.get() + r.radiation_w.get(),
        1e-8,
    );
}

#[test]
fn temperature_dependent_resistivity_gives_asymmetric_flux() {
    let mut s = segment();
    s.conductivity = k(1.0, 1.0, 100.0);
    s.length_m = Meters::new(1.0).unwrap();
    s.area_m2 = SquareMeters::new(1.0).unwrap();
    s.current_a = Amperes::new(1.0).unwrap();
    s.resistivity = Some(
        ResistivityCurve::table(vec![
            (Kelvin::new(1.0).unwrap(), OhmMeters::new(0.01).unwrap()),
            (Kelvin::new(100.0).unwrap(), OhmMeters::new(1.0).unwrap()),
        ])
        .unwrap(),
    );
    let r = solve(&s).unwrap();
    // T'' + 0.01 T = 0, T(0)=10, T(1)=20.
    let w: f64 = 0.1;
    let b = (20.0 - 10.0 * w.cos()) / w.sin();
    let start = w * b;
    let end = w * (10.0 * w.sin() - b * w.cos());
    near(r.heat_into_start_w.get(), start, 1e-6);
    near(r.heat_into_end_w.get(), end, 1e-6);
    near(r.joule_w.get(), start + end, 1e-6);
    assert!((r.heat_into_start_w.get() - 10.0 - r.joule_w.get() / 2.0).abs() > 0.005);
}

#[test]
fn invalid_geometry_missing_resistivity_and_extrapolation_fail() {
    let mut s = segment();
    s.length_m = Meters::new(0.0).unwrap();
    assert!(solve(&s).is_err());
    s = segment();
    s.area_m2 = SquareMeters::new(-1.0).unwrap();
    assert!(solve(&s).is_err());
    s = segment();
    s.current_a = Amperes::new(1.0).unwrap();
    assert!(solve(&s).is_err());
    s = segment();
    s.end_k = Kelvin::new(101.0).unwrap();
    assert!(solve(&s).is_err());
    s = segment();
    s.current_a = Amperes::new(1.0).unwrap();
    s.resistivity = Some(
        ResistivityCurve::table(vec![
            (Kelvin::new(11.0).unwrap(), OhmMeters::new(1.0).unwrap()),
            (Kelvin::new(21.0).unwrap(), OhmMeters::new(2.0).unwrap()),
        ])
        .unwrap(),
    );
    assert!(solve(&s).is_err());
    assert!(ResistivityCurve::table(vec![
        (Kelvin::new(2.0).unwrap(), OhmMeters::new(1.0).unwrap()),
        (Kelvin::new(1.0).unwrap(), OhmMeters::new(2.0).unwrap())
    ])
    .is_err());
    assert!(ResistivityCurve::table(vec![(
        Kelvin::new(1.0).unwrap(),
        OhmMeters::new(-1.0).unwrap()
    )])
    .is_err());
}

#[test]
fn lake_shore_copper_two_strands_reference() {
    // Lake Shore cryogenic wire conductivity table; linear interpolation in T.
    let points = [
        (0.1, 9.0),
        (0.4, 30.0),
        (1.0, 70.0),
        (4.0, 300.0),
        (10.0, 700.0),
        (20.0, 1100.0),
        (80.0, 600.0),
        (150.0, 410.0),
        (300.0, 400.0),
    ];
    let mut s = segment();
    s.conductivity = ThermalConductivityCurve::new(
        points
            .into_iter()
            .map(|(t, k)| {
                (
                    Kelvin::new(t).unwrap(),
                    WattsPerMeterKelvin::new(k).unwrap(),
                )
            })
            .collect(),
    )
    .unwrap();
    s.length_m = Meters::new(0.30).unwrap();
    s.area_m2 = SquareMeters::new(0.12e-6).unwrap();
    s.start_k = Kelvin::new(77.0).unwrap();
    s.end_k = Kelvin::new(4.2).unwrap();
    let r = solve(&s).unwrap();
    near(2.0 * r.heat_into_end_w.get(), 0.0488809333333, 1e-9);
    assert!((2.0 * r.heat_into_end_w.get() - 0.049).abs() < 0.049 * 0.15);
}

#[test]
fn radiation_at_equal_environment_and_wire_temperature_is_zero() {
    let mut s = segment();
    s.start_k = Kelvin::new(20.0).unwrap();
    s.end_k = Kelvin::new(20.0).unwrap();
    s.radiation = Some(cvtl_physics::wire::Radiation {
        emissivity: 0.8,
        surface_area_m2: SquareMeters::new(0.5).unwrap(),
        environment_k: Kelvin::new(20.0).unwrap(),
    });
    let r = solve(&s).unwrap();
    near(r.radiation_w.get(), 0.0, 1e-12);
    near(
        r.heat_into_start_w.get() + r.heat_into_end_w.get(),
        0.0,
        1e-12,
    );
}

#[test]
fn sensitivity_samples_nominal_and_four_corners() {
    let s = segment();
    let r = cvtl_physics::wire::sensitivity(&s, 0.1, 0.1).unwrap();
    assert_eq!(r.corners.len(), 4);
    near(r.nominal.heat_into_start_w.get(), 5.0, 1e-8);
    let mut end_flows: Vec<_> = r.corners.iter().map(|v| v.heat_into_end_w.get()).collect();
    end_flows.sort_by(f64::total_cmp);
    near(end_flows[0], -5.0 * 1.1 / 0.9, 1e-8);
    near(end_flows[3], -5.0 * 0.9 / 1.1, 1e-8);
}

#[test]
fn nonlinear_k_with_heating_matches_kirchhoff_solution() {
    let mut s = segment();
    s.conductivity = ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(1.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerMeterKelvin::new(100.0).unwrap(),
        ),
    ])
    .unwrap();
    s.length_m = Meters::new(1.0).unwrap();
    s.area_m2 = SquareMeters::new(1.0).unwrap();
    s.current_a = Amperes::new(1.0).unwrap();
    s.resistivity = Some(ResistivityCurve::constant(OhmMeters::new(2.0).unwrap()).unwrap());
    let r = solve(&s).unwrap();
    near(r.heat_into_start_w.get(), 151.0, 1e-5);
    near(r.heat_into_end_w.get(), -149.0, 1e-5);
}

#[test]
fn tiny_nonlinear_conduction_is_not_lost_to_absolute_tolerance() {
    let mut s = segment();
    s.conductivity = ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(1.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerMeterKelvin::new(100.0).unwrap(),
        ),
    ])
    .unwrap();
    s.area_m2 = SquareMeters::new(1e-14).unwrap();
    let r = solve(&s).unwrap();
    near(r.heat_into_start_w.get(), 7.5e-13, 1e-25);
}

#[test]
fn heat_can_enter_both_nodes_and_interior_domain_escape_fails() {
    let mut s = segment();
    s.start_k = Kelvin::new(20.0).unwrap();
    s.end_k = Kelvin::new(20.0).unwrap();
    s.current_a = Amperes::new(2.0).unwrap();
    s.resistivity = Some(ResistivityCurve::constant(OhmMeters::new(0.25).unwrap()).unwrap());
    let r = solve(&s).unwrap();
    near(r.heat_into_start_w.get(), 2.0, 1e-7);
    near(r.heat_into_end_w.get(), 2.0, 1e-7);
    s.resistivity = Some(ResistivityCurve::constant(OhmMeters::new(1000.0).unwrap()).unwrap());
    assert!(solve(&s).is_err());
}

#[test]
fn radiation_absorption_is_in_end_node_balance() {
    let mut s = segment();
    s.start_k = Kelvin::new(20.0).unwrap();
    s.end_k = Kelvin::new(20.0).unwrap();
    s.radiation = Some(cvtl_physics::wire::Radiation {
        emissivity: 0.8,
        surface_area_m2: SquareMeters::new(0.5).unwrap(),
        environment_k: Kelvin::new(80.0).unwrap(),
    });
    let r = solve(&s).unwrap();
    assert!(r.radiation_w.get() > 0.0);
    near(
        r.heat_into_start_w.get() + r.heat_into_end_w.get(),
        r.joule_w.get() + r.radiation_w.get(),
        1e-9,
    );
}
