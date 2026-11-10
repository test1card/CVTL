use cvtl_core::units::{JoulesPerKilogram, JoulesPerKilogramKelvin, Kelvin, WattsPerMeterKelvin};
use cvtl_materials::{HeatCapacityCurve, ThermalConductivityCurve};

fn t(x: f64) -> Kelvin {
    Kelvin::new(x).unwrap()
}
fn cp(x: f64) -> JoulesPerKilogramKelvin {
    JoulesPerKilogramKelvin::new(x).unwrap()
}
fn k(x: f64) -> WattsPerMeterKelvin {
    WattsPerMeterKelvin::new(x).unwrap()
}
fn near(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn piecewise_linear_cp_integral_and_inverse() {
    let c =
        HeatCapacityCurve::new(vec![(t(1.), cp(2.)), (t(3.), cp(6.)), (t(5.), cp(4.))]).unwrap();
    near(c.value(t(2.)).unwrap().get(), 4.);
    near(c.integral(t(1.), t(3.)).unwrap().get(), 8.);
    near(c.integral(t(2.), t(4.)).unwrap().get(), 10.5);
    near(c.integral(t(4.), t(2.)).unwrap().get(), -10.5);
    for x in [1., 1.5, 3., 4.5, 5.] {
        let h = c.integral(t(1.), t(x)).unwrap();
        near(c.inverse_integral(h).unwrap().get(), x);
    }
    assert!(c.value(t(0.5)).is_err());
    assert!(c.integral(t(1.), t(6.)).is_err());
    assert!(c
        .inverse_integral(JoulesPerKilogram::new(19.).unwrap())
        .is_err());
}

#[test]
fn conductivity_interpolates_and_integrates_with_sign() {
    let c = ThermalConductivityCurve::new(vec![(t(2.), k(3.)), (t(6.), k(7.))]).unwrap();
    near(c.value(t(4.)).unwrap().get(), 5.);
    near(c.integral(t(2.), t(6.)).unwrap().get(), 20.);
    near(c.integral(t(6.), t(2.)).unwrap().get(), -20.);
    assert!(c.value(t(7.)).is_err());
}

#[test]
fn rejects_invalid_tables() {
    assert!(HeatCapacityCurve::new(vec![]).is_err());
    assert!(HeatCapacityCurve::new(vec![(t(1.), cp(1.))]).is_err());
    assert!(HeatCapacityCurve::new(vec![(t(1.), cp(1.)), (t(1.), cp(2.))]).is_err());
    assert!(HeatCapacityCurve::new(vec![(t(2.), cp(1.)), (t(1.), cp(2.))]).is_err());
    assert!(HeatCapacityCurve::new(vec![(t(1.), cp(0.)), (t(2.), cp(2.))]).is_err());
    assert!(ThermalConductivityCurve::new(vec![(t(1.), k(1.)), (t(2.), k(0.))]).is_err());
}

// Independently evaluated NIST published fits at 20, 77 and 250 K (not measurements).
#[test]
fn nist_fit_checkpoints() {
    use cvtl_materials::{nist_conductivity, nist_heat_capacity};
    let cp = [
        ("ofhc_rrr100", [7.5060729, 195.9208752, 380.61587292]),
        ("al6061_t6", [8.85427029, 348.12792491, 895.0404244]),
        ("ss304", [13.45247806, 204.49302745, 457.91553453]),
        ("g10_cr", [47.17138783, 239.35266346, 836.33246105]),
    ];
    let ks = [
        ("ofhc_rrr100", [2422.51026454, 547.19969808, 400.88384294]),
        ("al6061_t6", [28.42754763, 83.53144195, 147.24268342]),
        ("al1100", [282.62328058, 290.17435084, 213.45610364]),
        ("ss304", [2.16862241, 7.9206516, 13.98124475]),
        ("g10_cr_normal", [0.15642187, 0.27996541, 0.52332247]),
        ("g10_cr_warp", [0.19817257, 0.38582223, 0.77208345]),
    ];
    for (id, values) in cp {
        let c = nist_heat_capacity(id).unwrap();
        for (temp, expected) in [20., 77., 250.].into_iter().zip(values) {
            let actual = c.curve.value(t(temp)).unwrap().get();
            assert!(
                (actual / expected - 1.).abs() < 0.00002,
                "{id}: {actual} vs {expected}"
            );
        }
    }
    for (id, values) in ks {
        let c = nist_conductivity(id).unwrap();
        for (temp, expected) in [20., 77., 250.].into_iter().zip(values) {
            let actual = c.curve.value(t(temp)).unwrap().get();
            assert!(
                (actual / expected - 1.).abs() < 0.00002,
                "{id}: {actual} vs {expected}"
            );
        }
    }
    assert!(nist_heat_capacity("al1100").is_err());
    assert!(nist_conductivity("g10_cr").is_err());
    assert!(nist_conductivity("g10_cr_normal")
        .unwrap()
        .curve
        .value(t(9.))
        .is_err());
    assert!(nist_conductivity("g10_cr_warp")
        .unwrap()
        .curve
        .value(t(11.))
        .is_err());
}

#[test]
fn extreme_finite_curve_does_not_panic_and_has_exact_endpoints() {
    let c = HeatCapacityCurve::new(vec![(t(1.), cp(1e300)), (t(2.), cp(1.5e300))]).unwrap();
    assert_eq!(c.value(t(1.)).unwrap().get(), 1e300);
    assert_eq!(c.value(t(2.)).unwrap().get(), 1.5e300);
    assert!(c.value(t(1.5)).unwrap().get().is_finite());
    let h = c.integral(t(1.), t(1.5)).unwrap();
    near(c.inverse_integral(h).unwrap().get(), 1.5);
    assert_eq!(
        c.inverse_integral(c.integral(t(1.), t(2.)).unwrap())
            .unwrap()
            .get(),
        2.
    );
}
