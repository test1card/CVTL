use cvtl_core::units::*;
use cvtl_materials::{HeatCapacityCurve, ThermalConductivityCurve};
use cvtl_physics::{balance, request, Body, Bridge, EmulationMode, InventoryPart, PortSample};

fn cp(v: f64) -> HeatCapacityCurve {
    HeatCapacityCurve::new(vec![
        (
            Kelvin::new(1.0).unwrap(),
            JoulesPerKilogramKelvin::new(v).unwrap(),
        ),
        (
            Kelvin::new(1000.0).unwrap(),
            JoulesPerKilogramKelvin::new(v).unwrap(),
        ),
    ])
    .unwrap()
}
fn k() -> ThermalConductivityCurve {
    ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(1.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(1000.0).unwrap(),
            WattsPerMeterKelvin::new(1000.0).unwrap(),
        ),
    ])
    .unwrap()
}
fn port(t: f64) -> PortSample {
    PortSample {
        temperature: Kelvin::new(t).unwrap(),
        age: Seconds::new(0.4).unwrap(),
    }
}
fn close(a: f64, b: f64, tol: f64) {
    assert!((a - b).abs() <= tol, "{a} != {b} (tol {tol})");
}

#[test]
fn exponential_cooldown_and_energy() {
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(2.0).unwrap(),
        1.0,
        Kelvin::new(100.0).unwrap(),
    )
    .unwrap();
    let bridge = Bridge::Constant(WattsPerKelvin::new(2.0).unwrap());
    let initial = body.enthalpy().get();
    let mut integrated = 0.0;
    for _ in 0..100 {
        integrated += body
            .step(
                Seconds::new(0.1).unwrap(),
                port(20.0),
                &bridge,
                Watts::new(0.0).unwrap(),
            )
            .unwrap()
            .net_energy
            .get();
    }
    let expected_temperature = 20.0 + 80.0 * (-1.0_f64).exp();
    close(body.temperature().get(), expected_temperature, 0.01);
    // The integral of the analytical heat flux is C times the temperature change.
    close(integrated, 20.0 * (expected_temperature - 100.0), 0.2);
    close(
        (body.enthalpy().get() - initial - integrated).abs() / integrated.abs(),
        0.0,
        1e-6,
    );
}

#[test]
fn signed_bridges_and_series_contact() {
    let constant = Bridge::Constant(WattsPerKelvin::new(2.0).unwrap());
    close(
        constant
            .flow(Kelvin::new(20.0).unwrap(), Kelvin::new(10.0).unwrap())
            .unwrap()
            .get(),
        20.0,
        1e-10,
    );
    close(
        constant
            .flow(Kelvin::new(10.0).unwrap(), Kelvin::new(20.0).unwrap())
            .unwrap()
            .get(),
        -20.0,
        1e-10,
    );
    let conductor = Bridge::Conductor {
        conductivity: k(),
        area: SquareMeters::new(1.0).unwrap(),
        length: Meters::new(1.0).unwrap(),
    };
    close(
        conductor
            .flow(Kelvin::new(20.0).unwrap(), Kelvin::new(10.0).unwrap())
            .unwrap()
            .get(),
        150.0,
        1e-8,
    );
    let table = Bridge::Table(vec![
        (Kelvin::new(1.0).unwrap(), WattsPerKelvin::new(1.0).unwrap()),
        (
            Kelvin::new(101.0).unwrap(),
            WattsPerKelvin::new(11.0).unwrap(),
        ),
    ]);
    close(
        table
            .flow(Kelvin::new(20.0).unwrap(), Kelvin::new(10.0).unwrap())
            .unwrap()
            .get(),
        24.0,
        1e-8,
    );
    let series = Bridge::Series {
        contact: WattsPerKelvin::new(10.0).unwrap(),
        conductivity: k(),
        area: SquareMeters::new(1.0).unwrap(),
        length: Meters::new(1.0).unwrap(),
    };
    // Interface x satisfies 10*(20-x) = (x²-10²)/2.
    let expected = 10.0 * (20.0 - (-10.0 + 600.0_f64.sqrt()));
    close(
        series
            .flow(Kelvin::new(20.0).unwrap(), Kelvin::new(10.0).unwrap())
            .unwrap()
            .get(),
        expected,
        1e-7,
    );
    close(
        series
            .flow(Kelvin::new(10.0).unwrap(), Kelvin::new(20.0).unwrap())
            .unwrap()
            .get(),
        10.0 * (10.0 - (-10.0 + 700.0_f64.sqrt())),
        1e-7,
    );
}

#[test]
fn stiff_step_remains_bounded_and_events_conserve_energy() {
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(100.0).unwrap(),
    )
    .unwrap();
    let h0 = body.enthalpy().get();
    let report = body
        .step(
            Seconds::new(10.0).unwrap(),
            port(20.0),
            &Bridge::Constant(WattsPerKelvin::new(10000.0).unwrap()),
            Watts::new(0.0).unwrap(),
        )
        .unwrap();
    assert!((20.0..=100.0).contains(&body.temperature().get()));
    let t = body.temperature().get();
    let event = body
        .reconfigure(cp(20.0), Kilograms::new(2.0).unwrap(), 0.5)
        .unwrap();
    close(body.temperature().get(), t, 1e-9);
    let residual = body.enthalpy().get() - h0 - report.net_energy.get() - event.get();
    close(
        residual.abs() / (report.net_energy.get().abs() + event.get().abs()),
        0.0,
        1e-6,
    );
}

#[test]
fn fixture_inventory_emulation_and_balance() {
    let parts = vec![
        InventoryPart {
            mass: Kilograms::new(2.0).unwrap(),
            temperature: Kelvin::new(50.0).unwrap(),
            heat_capacity: cp(10.0),
        },
        InventoryPart {
            mass: Kilograms::new(1.0).unwrap(),
            temperature: Kelvin::new(60.0).unwrap(),
            heat_capacity: cp(5.0),
        },
    ];
    let cf = cvtl_physics::fixture_heat_capacity(&parts).unwrap();
    close(cf.get(), 25.0, 1e-10);
    let a = request(EmulationMode::AdditionalLoad, Watts::new(10.0).unwrap()).unwrap();
    close(a.requested.get(), 10.0, 1e-10);
    assert!(!a.negative_clamped);
    let b = request(
        EmulationMode::FixtureCompensation {
            capacity: cf,
            temperature_rate: KelvinPerSecond::new(-1.0).unwrap(),
            parasitic: Watts::new(2.0).unwrap(),
        },
        Watts::new(10.0).unwrap(),
    )
    .unwrap();
    close(b.requested.get(), -17.0, 1e-10);
    close(b.nonnegative_target.get(), 0.0, 1e-10);
    assert!(b.negative_clamped);
    close(
        balance(
            &[Watts::new(10.0).unwrap(), Watts::new(5.0).unwrap()],
            Watts::new(1.0).unwrap(),
            Watts::new(2.0).unwrap(),
            Watts::new(3.0).unwrap(),
            cf,
            KelvinPerSecond::new(-1.0).unwrap(),
        )
        .unwrap()
        .get(),
        46.0,
        1e-10,
    );
}

#[test]
fn invalid_edits_and_steps_are_atomic() {
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(100.0).unwrap(),
    )
    .unwrap();
    let initial = body.enthalpy().get();
    assert!(body
        .reconfigure(cp(10.0), Kilograms::new(0.0).unwrap(), 1.0)
        .is_err());
    assert_eq!(body.enthalpy().get(), initial);
    assert!(body
        .step(
            Seconds::new(1.0).unwrap(),
            PortSample {
                temperature: Kelvin::new(20.0).unwrap(),
                age: Seconds::new(-1.0).unwrap()
            },
            &Bridge::Constant(WattsPerKelvin::new(1.0).unwrap()),
            Watts::new(0.0).unwrap()
        )
        .is_err());
    assert_eq!(body.enthalpy().get(), initial);
    assert!(body
        .step(
            Seconds::new(1.0).unwrap(),
            port(20.0),
            &Bridge::Constant(WattsPerKelvin::new(1.0).unwrap()),
            Watts::new(1.0e9).unwrap()
        )
        .is_err());
    assert_eq!(body.enthalpy().get(), initial);
}

#[test]
fn narrow_bridge_domains_and_zero_conductance() {
    let narrow = ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(10.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
    ])
    .unwrap();
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(50.0).unwrap(),
    )
    .unwrap();
    let conductor = Bridge::Conductor {
        conductivity: narrow,
        area: SquareMeters::new(1.0).unwrap(),
        length: Meters::new(1.0).unwrap(),
    };
    body.step(
        Seconds::new(1.0).unwrap(),
        port(20.0),
        &conductor,
        Watts::new(0.0).unwrap(),
    )
    .unwrap();
    let table = Bridge::Table(vec![
        (
            Kelvin::new(10.0).unwrap(),
            WattsPerKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerKelvin::new(2.0).unwrap(),
        ),
    ]);
    body.step(
        Seconds::new(1.0).unwrap(),
        port(20.0),
        &table,
        Watts::new(0.0).unwrap(),
    )
    .unwrap();
    let t = body.temperature().get();
    let h = body.enthalpy().get();
    let step = body
        .step(
            Seconds::new(1.0).unwrap(),
            port(20.0),
            &Bridge::Constant(WattsPerKelvin::new(0.0).unwrap()),
            Watts::new(0.0).unwrap(),
        )
        .unwrap();
    close(body.temperature().get(), t, 1e-9);
    close(step.net_energy.get(), 0.0, 1e-9);
    close(body.enthalpy().get(), h, 1e-9);
}

#[test]
fn variable_cp_heating_and_representability() {
    let curve = HeatCapacityCurve::new(vec![
        (
            Kelvin::new(1.0).unwrap(),
            JoulesPerKilogramKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(1000.0).unwrap(),
            JoulesPerKilogramKelvin::new(1000.0).unwrap(),
        ),
    ])
    .unwrap();
    let mut body = Body::new(
        curve.clone(),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(10.0).unwrap(),
    )
    .unwrap();
    body.step(
        Seconds::new(10.0).unwrap(),
        port(20.0),
        &Bridge::Constant(WattsPerKelvin::new(0.0).unwrap()),
        Watts::new(10.0).unwrap(),
    )
    .unwrap();
    close(body.temperature().get(), 300.0_f64.sqrt(), 1e-6);
    let h = body.enthalpy().get();
    assert!(body
        .reconfigure(
            curve.clone(),
            Kilograms::new(f64::MIN_POSITIVE).unwrap(),
            f64::MIN_POSITIVE
        )
        .is_err());
    assert_eq!(body.enthalpy().get(), h);
    assert!(Body::new(
        curve,
        Kilograms::new(f64::MAX).unwrap(),
        1.0,
        Kelvin::new(100.0).unwrap()
    )
    .is_err());
}

#[test]
fn nonmonotone_table_flow_has_stable_implicit_branch() {
    let table = Bridge::Table(vec![
        (
            Kelvin::new(1.0).unwrap(),
            WattsPerKelvin::new(100.0).unwrap(),
        ),
        (
            Kelvin::new(51.0).unwrap(),
            WattsPerKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(501.0).unwrap(),
            WattsPerKelvin::new(1.0).unwrap(),
        ),
    ]);
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(100.0).unwrap(),
    )
    .unwrap();
    body.step(
        Seconds::new(0.5).unwrap(),
        port(2.0),
        &table,
        Watts::new(0.0).unwrap(),
    )
    .unwrap();
    assert!((2.0..100.0).contains(&body.temperature().get()));
}

#[test]
fn bridge_domain_is_checked_at_start() {
    let narrow = ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(10.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
    ])
    .unwrap();
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(150.0).unwrap(),
    )
    .unwrap();
    let bridge = Bridge::Conductor {
        conductivity: narrow,
        area: SquareMeters::new(1.0).unwrap(),
        length: Meters::new(1.0).unwrap(),
    };
    let h = body.enthalpy().get();
    assert!(body
        .step(
            Seconds::new(1000.0).unwrap(),
            port(20.0),
            &bridge,
            Watts::new(0.0).unwrap()
        )
        .is_err());
    assert_eq!(body.enthalpy().get(), h);
    let table = Bridge::Table(vec![
        (
            Kelvin::new(10.0).unwrap(),
            WattsPerKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(50.0).unwrap(),
            WattsPerKelvin::new(1.0).unwrap(),
        ),
    ]);
    assert!(body
        .step(
            Seconds::new(1000.0).unwrap(),
            port(20.0),
            &table,
            Watts::new(0.0).unwrap()
        )
        .is_err());
    assert_eq!(body.enthalpy().get(), h);
}

#[test]
fn series_allows_body_outside_conductor_range() {
    let narrow = ThermalConductivityCurve::new(vec![
        (
            Kelvin::new(10.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
        (
            Kelvin::new(100.0).unwrap(),
            WattsPerMeterKelvin::new(1.0).unwrap(),
        ),
    ])
    .unwrap();
    let bridge = Bridge::Series {
        contact: WattsPerKelvin::new(1.0).unwrap(),
        conductivity: narrow,
        area: SquareMeters::new(1.0).unwrap(),
        length: Meters::new(1.0).unwrap(),
    };
    close(
        bridge
            .flow(Kelvin::new(150.0).unwrap(), Kelvin::new(20.0).unwrap())
            .unwrap()
            .get(),
        65.0,
        1e-8,
    );
    let mut body = Body::new(
        cp(10.0),
        Kilograms::new(1.0).unwrap(),
        1.0,
        Kelvin::new(150.0).unwrap(),
    )
    .unwrap();
    body.step(
        Seconds::new(1.0).unwrap(),
        port(20.0),
        &bridge,
        Watts::new(0.0).unwrap(),
    )
    .unwrap();
}
