use cvtl_core::units::{Kelvin, Kilograms, Meters, Seconds, SquareMeters, Watts};
use cvtl_materials::{nist_conductivity, nist_heat_capacity};
use cvtl_physics::{Body, Bridge, PortSample};

#[test]
fn copper_body_releases_heat_through_a_material_bridge() {
    let cp = nist_heat_capacity("ofhc_rrr100").unwrap().curve;
    let conductivity = nist_conductivity("ofhc_rrr100").unwrap().curve;
    let mut body = Body::new(
        cp,
        Kilograms::new(20.0).unwrap(),
        0.5,
        Kelvin::new(80.0).unwrap(),
    )
    .unwrap();
    let bridge = Bridge::Conductor {
        conductivity,
        area: SquareMeters::new(1.0e-5).unwrap(),
        length: Meters::new(0.1).unwrap(),
    };
    let sample = PortSample {
        temperature: Kelvin::new(20.0).unwrap(),
        age: Seconds::new(0.2).unwrap(),
    };
    let initial_enthalpy = body.enthalpy().get();
    let initial_flow = bridge.flow(body.temperature(), sample.temperature).unwrap();
    let duration = Seconds::new(5.0).unwrap();
    let step = body
        .step(duration, sample, &bridge, Watts::new(0.0).unwrap())
        .unwrap();
    let final_flow = bridge.flow(body.temperature(), sample.temperature).unwrap();
    let released = -step.net_energy.get();

    assert!(body.temperature() < Kelvin::new(80.0).unwrap());
    assert!(final_flow.get() > 0.0 && final_flow < initial_flow);
    assert!(released >= final_flow.get() * duration.get());
    assert!(released <= initial_flow.get() * duration.get());
    let residual = (body.enthalpy().get() - initial_enthalpy + released).abs();
    assert!(
        residual / released <= 1.0e-6,
        "energy residual {residual} J"
    );
}
