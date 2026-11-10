use cvtl_core::units::Kelvin;
use cvtl_materials::lakeshore_wire_conductivity;

#[test]
fn published_wire_curves_preserve_values_and_domains() {
    // Lower bound and conductivity at the lower bound, 20 K, and 300 K.
    for (id, low, first, at_20, last) in [
        ("lakeshore_copper", 0.1, 9.0, 1100.0, 400.0),
        ("lakeshore_phosphor_bronze", 1.0, 0.22, 10.0, 48.0),
        ("lakeshore_nichrome", 4.0, 0.25, 2.6, 12.0),
        ("lakeshore_manganin", 0.1, 0.006, 3.3, 22.0),
    ] {
        let curve = lakeshore_wire_conductivity(id).unwrap();
        let (min, max) = curve.bounds();
        assert_eq!((min.get(), max.get()), (low, 300.0), "{id}");
        for (temperature, expected) in [
            (min, first),
            (Kelvin::new(20.0).unwrap(), at_20),
            (max, last),
        ] {
            assert_eq!(curve.value(temperature).unwrap().get(), expected, "{id}");
        }
        assert!(curve.value(Kelvin::new(low * 0.9).unwrap()).is_err());
        assert!(curve.value(Kelvin::new(301.0).unwrap()).is_err());
        let scaled = curve.scaled(1.1).unwrap();
        assert_eq!(scaled.bounds(), curve.bounds());
        let value = scaled.value(Kelvin::new(20.0).unwrap()).unwrap().get();
        assert!((value - at_20 * 1.1).abs() < 1e-12, "{id}");
    }
}
