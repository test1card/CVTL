use cvtl_core::units::{Kelvin, Kilograms, Meters, SquareMeters, WattsPerKelvin};
use cvtl_materials::{nist_conductivity, nist_heat_capacity};
use cvtl_physics::{
    scenario::{BodySetup, BridgeSetup},
    Bridge,
};

const CATALOG: &str = include_str!("../../../data/materials/nist.csv");
const COPPER: &str = "ofhc_rrr100";
const AREA: f64 = 1e-6;
const LENGTH: f64 = 0.10;

fn k(value: f64) -> Kelvin {
    Kelvin::new(value).unwrap()
}

fn g(value: f64) -> WattsPerKelvin {
    WattsPerKelvin::new(value).unwrap()
}

fn setup(bridge: BridgeSetup) -> BodySetup {
    BodySetup {
        material: COPPER.into(),
        mass_kg: 20.0,
        temperature_k: 30.0,
        scale: 0.1,
        bridge,
    }
}

#[test]
fn builds_each_existing_bridge_from_one_setup() {
    let conductivity = nist_conductivity(COPPER).unwrap().curve;
    let area = SquareMeters::new(AREA).unwrap();
    let length = Meters::new(LENGTH).unwrap();
    let points = [[4.0, 0.02], [300.0, 0.03]];
    let cases = [
        (
            BridgeSetup::Constant {
                conductance_w_per_k: 0.02,
            },
            Bridge::Constant(g(0.02)),
        ),
        (
            BridgeSetup::Conductor {
                material: COPPER.into(),
                area_m2: AREA,
                length_m: LENGTH,
            },
            Bridge::Conductor {
                conductivity: conductivity.clone(),
                area,
                length,
            },
        ),
        (
            BridgeSetup::Table {
                points: points.to_vec(),
            },
            Bridge::Table(points.map(|[t, value]| (k(t), g(value))).to_vec()),
        ),
        (
            BridgeSetup::Series {
                contact_w_per_k: 0.04,
                material: COPPER.into(),
                area_m2: AREA,
                length_m: LENGTH,
            },
            Bridge::Series {
                contact: g(0.04),
                conductivity,
                area,
                length,
            },
        ),
    ];
    for (configured, reference) in cases {
        let (body, bridge) = setup(configured).build(CATALOG, k(16.0)).unwrap();
        assert!((body.temperature().get() - 30.0).abs() < 1e-9);
        let q = bridge.flow(body.temperature(), k(16.0)).unwrap().get();
        let expected = reference.flow(k(30.0), k(16.0)).unwrap().get();
        assert!((q - expected).abs() < 1e-12);
    }
}

#[test]
fn setup_rejects_invalid_properties_and_initial_ranges() {
    let constant = BridgeSetup::Constant {
        conductance_w_per_k: 0.02,
    };
    let mut invalid = setup(constant.clone());
    invalid.material = "unknown".into();
    assert!(invalid.build(CATALOG, k(16.0)).is_err());
    for (mass, scale, temperature) in [(0.0, 0.1, 30.0), (20.0, 1.1, 30.0), (20.0, 0.1, 301.0)] {
        let mut invalid = setup(constant.clone());
        invalid.mass_kg = mass;
        invalid.scale = scale;
        invalid.temperature_k = temperature;
        assert!(invalid.build(CATALOG, k(16.0)).is_err());
    }
    for bridge in [
        BridgeSetup::Constant {
            conductance_w_per_k: -1.0,
        },
        BridgeSetup::Conductor {
            material: COPPER.into(),
            area_m2: 0.0,
            length_m: LENGTH,
        },
        BridgeSetup::Series {
            contact_w_per_k: 0.0,
            material: COPPER.into(),
            area_m2: AREA,
            length_m: LENGTH,
        },
        BridgeSetup::Table {
            points: vec![[20.0, 0.1], [20.0, 0.2]],
        },
        BridgeSetup::Table {
            points: vec![[40.0, 0.1], [50.0, 0.2]],
        },
    ] {
        assert!(setup(bridge).build(CATALOG, k(16.0)).is_err());
    }
    let conductor = BridgeSetup::Conductor {
        material: COPPER.into(),
        area_m2: AREA,
        length_m: LENGTH,
    };
    assert!(setup(conductor).build(CATALOG, k(3.0)).is_err());
}

#[test]
fn one_mass_and_scale_change_preserves_temperature_and_has_signed_enthalpy() {
    let (mut body, _) = setup(BridgeSetup::Constant {
        conductance_w_per_k: 0.02,
    })
    .build(CATALOG, k(16.0))
    .unwrap();
    let temperature = body.temperature();
    let before = body.enthalpy();
    assert!(body
        .reconfigure_mass_scale(Kilograms::new(0.0), Some(0.2))
        .is_err());
    assert_eq!(body.enthalpy(), before);
    assert_eq!(body.temperature(), temperature);
    let cp = nist_heat_capacity(COPPER).unwrap().curve;
    let integral = cp.integral(cp.bounds().0, temperature).unwrap().get();
    let change = body
        .reconfigure_mass_scale(Kilograms::new(10.0), Some(0.15))
        .unwrap();
    assert!(change.get() < 0.0);
    assert!((change.get() - (10.0 * 0.15 - 20.0 * 0.1) * integral).abs() < 1e-6);
    assert!((body.temperature().get() - temperature.get()).abs() < 1e-6);
    let scale_only = body.reconfigure_mass_scale(None, Some(0.2)).unwrap();
    assert!((scale_only.get() - (10.0 * 0.2 - 10.0 * 0.15) * integral).abs() < 1e-6);
}
