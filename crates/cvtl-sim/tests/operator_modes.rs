use std::time::Duration;

use cvtl_core::units::{Kelvin, Seconds, Watts};
use cvtl_materials::nist_heat_capacity_from_catalog;
use cvtl_physics::{
    request,
    scenario::{BodySetup, BridgeSetup},
    EmulationMode,
};
use cvtl_sim::modes::{
    compensation_emulation, control_demand, BodyEdit, BodyPort, Compensation, CompensationSetup,
    FixturePartSetup, HeadSetup, HeldPort, ModeSetup, ModeSnapshot, PowerPoint, VirtualBody,
    CATALOG,
};
use cvtl_store::record::{Demand, ModeChangeOutcomeV1, Quality, TemperatureSample, ThermalPoint};

fn profile(points: &[(u64, f64)]) -> ModeSetup {
    ModeSetup::PowerProfile {
        points: points
            .iter()
            .map(|&(elapsed_ms, power_w)| PowerPoint {
                elapsed_ms,
                power_w,
            })
            .collect(),
    }
}

#[test]
fn power_profile_interpolates_between_exact_knots_and_holds_its_end() {
    let mode = profile(&[(0, 0.75), (1000, 1.25), (2000, 0.75)]);
    assert!(mode.validate().is_ok());
    for (elapsed_ms, expected_w) in [
        (0, 0.75_f64),
        (500, 1.0),
        (1000, 1.25),
        (1500, 1.0),
        (2000, 0.75),
        (3000, 0.75),
    ] {
        let power = mode.power_at(elapsed_ms * 1_000_000).unwrap();
        assert_eq!(power.get().to_bits(), expected_w.to_bits());
    }
    assert_eq!(
        profile(&[(0, 1.25)]).power_at(u64::MAX).unwrap().get(),
        1.25
    );
}

#[test]
fn invalid_power_profiles_are_rejected_before_use() {
    for mode in [
        profile(&[]),
        profile(&[(1, 0.75)]),
        profile(&[(0, 0.75), (0, 1.25)]),
        profile(&[(0, 0.75), (2000, 1.25), (1000, 1.0)]),
        profile(&[(0, 0.75), (u64::MAX, 1.25)]),
        profile(&[(0, -0.75)]),
        profile(&[(0, f64::NAN)]),
        profile(&[(0, f64::INFINITY)]),
    ] {
        assert!(mode.validate().is_err(), "{mode:?}");
    }
}

fn temperatures() -> [TemperatureSample; 4] {
    [
        ThermalPoint::Stage1,
        ThermalPoint::Stage2,
        ThermalPoint::Tf,
        ThermalPoint::Jacket,
    ]
    .map(|point| match point {
        ThermalPoint::Stage1 => (point, 1, 20.0, 1_000_000_000),
        ThermalPoint::Stage2 => (point, 2, 15.0, 1_100_000_000),
        ThermalPoint::Tf => (point, 3, 15.75, 1_120_000_000),
        ThermalPoint::Jacket => (point, 4, 77.0, 1_130_000_000),
    })
    .map(|(point, channel, temperature_k, stamp)| TemperatureSample {
        point,
        channel: Some(channel),
        quality: Quality::Measured,
        temperature_k: Some(temperature_k),
        read_started_ns: Some(stamp),
        read_finished_ns: Some(stamp + 1_000_000),
    })
}

#[test]
fn power_profile_is_relative_to_the_accepted_start() {
    let mode = profile(&[(0, 0.75), (1000, 1.25), (2000, 0.75)]);
    let frame = temperatures();
    assert_eq!(
        mode.ordinary_demand(None, 4_000_000_000, &frame).unwrap(),
        Demand::Observe
    );
    assert_eq!(
        mode.ordinary_demand(Some(1_000_000_000), 1_500_000_000, &frame)
            .unwrap(),
        Demand::PowerProfile { requested_w: 1.0 },
    );
    assert_eq!(
        mode.ordinary_demand(Some(1_500_000_000), 1_500_000_000, &frame)
            .unwrap(),
        Demand::PowerProfile { requested_w: 0.75 },
    );
    assert!(mode
        .ordinary_demand(Some(2_000_000_000), 1_500_000_000, &frame)
        .is_err());
}

#[test]
fn hold_keeps_the_same_frame_pair_and_its_earliest_stamp() {
    let mode = ModeSetup::TemperatureHold { setpoint_k: 15.2 };
    let mut frame = temperatures();
    // Both readings are stale at the M5 cutoff. The pair must reach M5 intact.
    let raw = mode
        .ordinary_demand(Some(0), 1_400_000_000, &frame)
        .unwrap();
    assert_eq!(
        raw,
        Demand::TemperatureHold {
            setpoint_k: 15.2,
            stage1_k: 20.0,
            stage2_k: 15.0,
            at_ns: 1_000_000_000,
        }
    );
    let cvtl_control::Demand::TemperatureHold {
        setpoint,
        stage1,
        stage2,
        at,
    } = control_demand(raw).unwrap()
    else {
        panic!("expected temperature hold demand");
    };
    assert_eq!(
        [setpoint.get(), stage1.get(), stage2.get(), at.get()],
        [15.2, 20.0, 15.0, 1.0]
    );
    frame[1].quality = Quality::Missing;
    frame[1].temperature_k = None;
    assert_eq!(
        mode.ordinary_demand(Some(0), 1_400_000_000, &frame)
            .unwrap(),
        Demand::Observe
    );
}

#[test]
fn hold_has_an_explicit_test_head_and_freshness_cutoff() {
    let mode = ModeSetup::TemperatureHold { setpoint_k: 15.2 };
    let head = HeadSetup::for_mode(&mode);
    let initial = head.build().unwrap().temperatures();
    assert_eq!(
        [initial.stage1.get(), initial.stage2.get(), initial.tf.get()],
        [20.0, 15.0, 15.75]
    );
    assert_eq!(head.capacity_j_per_k, [10.0, 2.0, 2.0]);
    assert_eq!(head.links_w_per_k, [0.0, 1.0]);
    assert_eq!(head.removal_w, [0.0, 0.75]);
    let hold = mode.hold_config().unwrap();
    assert_eq!([hold.kp.get(), hold.ki.get()], [2.0, 0.5]);
    assert_eq!(mode.temperature_age().as_millis(), 150);
    assert_eq!(ModeSetup::Observe.temperature_age().as_millis(), 1500);
}

#[test]
fn mode_snapshot_roundtrips_and_rejects_a_different_hold_head() {
    let snapshot = ModeSnapshot::new(ModeSetup::TemperatureHold { setpoint_k: 15.2 }).unwrap();
    let yaml = yaml_serde::to_string(&snapshot).unwrap();
    let mut decoded: ModeSnapshot = yaml_serde::from_str(&yaml).unwrap();
    decoded.validate().unwrap();

    decoded.head.initial_k[1] = 15.2;
    assert!(decoded.validate().is_err());
    decoded.head = snapshot.head;
    decoded.hold.as_mut().unwrap().kp_w_per_k = 3.0;
    assert!(decoded.validate().is_err());
    decoded.hold = snapshot.hold;
    decoded.mode = ModeSetup::Observe;
    assert!(decoded.validate().is_err());
}

#[test]
fn a_virtual_body_selection_defaults_to_tf_without_compensation() {
    let mode: ModeSetup = yaml_serde::from_str("kind: virtual_body\n").unwrap();
    assert_eq!(
        mode,
        ModeSetup::VirtualBody {
            port: BodyPort::Tf,
            body_heat_w: 0.0,
            compensation: None
        }
    );
    mode.validate().unwrap();
}

const BODY_AGE: Duration = Duration::from_millis(200);
const BODY_STEP_NS: u64 = 100_000_000;
const MEASURED_NS: u64 = 1_140_000_000;
const END_NS: u64 = MEASURED_NS + BODY_STEP_NS;

fn body_setup() -> BodySetup {
    BodySetup {
        material: "ofhc_rrr100".into(),
        mass_kg: 20.0,
        scale: 0.1,
        temperature_k: 30.0,
        bridge: BridgeSetup::Constant {
            conductance_w_per_k: 0.0,
        },
    }
}

fn body_mode(port: BodyPort, body_heat_w: f64) -> ModeSetup {
    ModeSetup::VirtualBody {
        port,
        body_heat_w,
        compensation: None,
    }
}

fn body(port: BodyPort, body_heat_w: f64) -> VirtualBody {
    let snapshot = ModeSnapshot::with_body(body_mode(port, body_heat_w), body_setup()).unwrap();
    let mut state = VirtualBody::new(&snapshot, CATALOG.into()).unwrap();
    let held = state
        .sample(&temperatures(), MEASURED_NS, BODY_AGE)
        .unwrap();
    state.observe(held);
    state
}

fn tf_frame(temperature_k: f64, read_ns: u64) -> [TemperatureSample; 4] {
    let mut frame = temperatures();
    frame[2].temperature_k = Some(temperature_k);
    frame[2].read_started_ns = Some(read_ns);
    frame[2].read_finished_ns = Some(read_ns + 1);
    frame
}

fn tf_at(state: &VirtualBody, temperature_k: f64, read_ns: u64) -> HeldPort {
    state
        .sample(&tf_frame(temperature_k, read_ns), read_ns + 1, BODY_AGE)
        .unwrap()
}

fn enthalpy(material: &str, mass_kg: f64, scale: f64, temperature_k: f64) -> f64 {
    let curve = nist_heat_capacity_from_catalog(material, CATALOG)
        .unwrap()
        .curve;
    mass_kg
        * scale
        * curve
            .integral(curve.bounds().0, Kelvin::new(temperature_k).unwrap())
            .unwrap()
            .get()
}

#[test]
fn only_virtual_body_snapshots_carry_a_body_setup() {
    assert!(ModeSnapshot::new(body_mode(BodyPort::Tf, 0.0)).is_err());
    assert!(ModeSnapshot::with_body(ModeSetup::Observe, body_setup()).is_err());
    let ordinary = ModeSnapshot::new(ModeSetup::Observe).unwrap();
    assert!(!yaml_serde::to_string(&ordinary).unwrap().contains("body:"));

    let snapshot =
        ModeSnapshot::with_body(body_mode(BodyPort::Stage2, 0.125), body_setup()).unwrap();
    let yaml = yaml_serde::to_string(&snapshot).unwrap();
    let mut decoded: ModeSnapshot = yaml_serde::from_str(&yaml).unwrap();
    assert!(VirtualBody::new(&decoded, CATALOG.into()).is_ok());
    decoded.body = None;
    assert!(decoded.validate().is_err());
}

#[test]
fn body_step_uses_the_selected_previously_held_port_and_exact_read_age() {
    for (port, point, index, held_k, read_ns) in [
        (
            BodyPort::Stage2,
            ThermalPoint::Stage2,
            1,
            15.0,
            1_100_000_000,
        ),
        (BodyPort::Tf, ThermalPoint::Tf, 2, 15.75, 1_120_000_000),
    ] {
        let mut state = body(port, 0.125);
        let before_j = state.enthalpy().get();
        let step = state.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
        assert_eq!(step.port, point);
        assert_eq!(step.held_k, held_k);
        assert_eq!(step.held_read_started_ns, read_ns);
        assert_eq!(step.held_age_ns, END_NS - read_ns);
        assert!((state.enthalpy().get() - before_j - 0.0125).abs() < 1e-10);
        assert!(state
            .advance(BODY_STEP_NS, read_ns + 200_000_001, BODY_AGE)
            .is_err());

        let mut frame = temperatures();
        frame[index].channel = Some(4);
        assert!(state.sample(&frame, MEASURED_NS, BODY_AGE).is_none());
        frame = temperatures();
        frame[index].quality = Quality::Invalid;
        assert!(state.sample(&frame, MEASURED_NS, BODY_AGE).is_none());
        frame = temperatures();
        frame[index].read_finished_ns = Some(MEASURED_NS + 1);
        assert!(state.sample(&frame, MEASURED_NS, BODY_AGE).is_none());
        assert!(state
            .sample(&temperatures(), read_ns + 200_000_000, BODY_AGE)
            .is_some());
        assert!(state
            .sample(&temperatures(), read_ns + 200_000_001, BODY_AGE)
            .is_none());
    }
}

#[test]
fn edit_applies_after_the_elapsed_interval_and_uses_the_current_port() {
    let mut state = body(BodyPort::Tf, 0.125);
    let mut step = state.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
    let before_k = step.temperature_k;
    let before_j = state.enthalpy().get();
    let edit = BodyEdit {
        material: Some("al6061_t6".into()),
        mass_kg: Some(2.0),
        scale: Some(0.2),
        bridge: Some(BridgeSetup::Constant {
            conductance_w_per_k: 0.05,
        }),
        ..BodyEdit::default()
    };
    state.validate_edit(&edit).unwrap();
    let raw = serde_json::to_value(&edit).unwrap();
    let current = tf_at(&state, 35.0, END_NS);
    let (scale, requested) = state
        .complete_tick(
            &mut step,
            Some(current),
            Some(edit),
            Some(EmulationMode::AdditionalLoad),
        )
        .unwrap();
    let change = step.change.unwrap();
    assert_eq!(change.request, raw);
    let ModeChangeOutcomeV1::Applied {
        temperature_before_k,
        temperature_after_k,
        dh_event_j: Some(dh_event_j),
    } = change.outcome
    else {
        panic!("expected an enthalpy-changing edit");
    };
    assert_eq!(temperature_before_k.to_bits(), before_k.to_bits());
    assert!((temperature_after_k - before_k).abs() < 1e-9);
    assert_eq!(step.temperature_k.to_bits(), temperature_after_k.to_bits());
    let after_j = enthalpy("al6061_t6", 2.0, 0.2, before_k);
    assert!((dh_event_j - (after_j - before_j)).abs() < 1e-10);
    assert!((scale - before_j.abs().max(after_j.abs())).abs() < 1e-10);
    let q_v = step.q_v_w.unwrap();
    assert_eq!(requested, Some(q_v));
    assert!(q_v < 0.0);
    assert!((q_v - 0.05 * (temperature_after_k - 35.0)).abs() < 1e-12);
    // complete_tick must not promote the new port before recording accepts it.
    let next = state
        .advance(10_000_000, END_NS + 10_000_000, BODY_AGE)
        .unwrap();
    assert_eq!(next.held_read_started_ns, 1_120_000_000);
    state.observe(current);
    let next = state
        .advance(10_000_000, END_NS + 20_000_000, BODY_AGE)
        .unwrap();
    assert_eq!(next.held_read_started_ns, END_NS);
    assert_eq!(next.held_k, 35.0);
}

#[test]
fn unavailable_or_out_of_domain_current_ports_reject_the_entire_edit() {
    let state = body(BodyPort::Tf, 0.0);
    for invalid in [
        BodyEdit::default(),
        BodyEdit {
            mass_kg: Some(2.0),
            material: Some("unknown".into()),
            ..BodyEdit::default()
        },
        BodyEdit {
            bridge: Some(body_setup().bridge),
            g_w_per_k: Some(0.02),
            ..BodyEdit::default()
        },
    ] {
        assert!(state.validate_edit(&invalid).is_err());
    }

    for has_current in [true, false] {
        let mut state = body(BodyPort::Tf, 0.0);
        let edit = BodyEdit {
            mass_kg: Some(2.0),
            bridge: Some(BridgeSetup::Table {
                points: vec![[22.8, 0.01], [22.95, 0.02]],
            }),
            ..BodyEdit::default()
        };
        state.validate_edit(&edit).unwrap();
        let mut step = state.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
        let h = state.enthalpy().get();
        let t = step.temperature_k;
        let current = has_current.then(|| tf_at(&state, 14.0, END_NS));
        let rejected = serde_json::to_value(&edit).unwrap();
        state
            .complete_tick(
                &mut step,
                current,
                Some(edit),
                Some(EmulationMode::AdditionalLoad),
            )
            .unwrap();
        let change = step.change.unwrap();
        assert_eq!(change.request, rejected);
        assert_eq!(change.outcome, ModeChangeOutcomeV1::Rejected);
        assert_eq!(step.q_v_w, has_current.then_some(0.0));
        assert_eq!(state.enthalpy().get().to_bits(), h.to_bits());
        assert_eq!(step.temperature_k.to_bits(), t.to_bits());
    }
}

#[test]
fn all_bridge_edits_preserve_enthalpy_exactly() {
    let constant = BodyEdit {
        g_w_per_k: Some(0.02),
        ..BodyEdit::default()
    };
    let cases = [
        BridgeSetup::Constant {
            conductance_w_per_k: 0.02,
        },
        BridgeSetup::Conductor {
            material: "ofhc_rrr100".into(),
            area_m2: 1e-6,
            length_m: 0.1,
        },
        BridgeSetup::Table {
            points: vec![[4.0, 0.01], [300.0, 0.02]],
        },
        BridgeSetup::Series {
            material: "ofhc_rrr100".into(),
            area_m2: 1e-6,
            length_m: 0.1,
            contact_w_per_k: 0.04,
        },
    ];
    for (index, bridge) in cases.into_iter().enumerate() {
        let edit = if index == 0 {
            constant.clone()
        } else {
            BodyEdit {
                bridge: Some(bridge.clone()),
                ..BodyEdit::default()
            }
        };
        let mut state = body(BodyPort::Tf, 0.0);
        state.validate_edit(&edit).unwrap();
        let mut step = state.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
        let h = state.enthalpy().get();
        let t = step.temperature_k;
        let current = tf_at(&state, 14.0, END_NS);
        state
            .complete_tick(
                &mut step,
                Some(current),
                Some(edit),
                Some(EmulationMode::AdditionalLoad),
            )
            .unwrap();
        assert!(matches!(
            step.change.unwrap().outcome,
            ModeChangeOutcomeV1::Applied {
                dh_event_j: None,
                ..
            }
        ));
        assert_eq!(state.enthalpy().get().to_bits(), h.to_bits());
        assert_eq!(step.temperature_k.to_bits(), t.to_bits());
        let expected = bridge
            .build(CATALOG)
            .unwrap()
            .flow(Kelvin::new(t).unwrap(), Kelvin::new(14.0).unwrap())
            .unwrap();
        assert!((step.q_v_w.unwrap() - expected.get()).abs() < 1e-12);
    }
}

fn compensated_snapshot(port: BodyPort, body_heat_w: f64, fixture_mass_kg: f64) -> ModeSnapshot {
    let mut body = body_setup();
    body.bridge = BridgeSetup::Constant {
        conductance_w_per_k: 0.02,
    };
    ModeSnapshot::with_body(
        ModeSetup::VirtualBody {
            port,
            body_heat_w,
            compensation: Some(CompensationSetup {
                fixture_parasitic_w: 0.05,
                fixture: vec![FixturePartSetup {
                    material: "ofhc_rrr100".into(),
                    mass_kg: fixture_mass_kg,
                }],
            }),
        },
        body,
    )
    .unwrap()
}

fn sample_tf(temperature_k: f64, stamp: u64) -> HeldPort {
    Compensation::sample(&tf_frame(temperature_k, stamp), stamp + 1, BODY_AGE).unwrap()
}

#[test]
fn compensation_matches_the_analytic_request_for_both_body_ports() {
    const FIXTURE_CF: f64 = 0.729_087_800_758;
    const EXPECTED_REQUEST: f64 = 0.157_091_219_924;
    let previous = sample_tf(16.01, MEASURED_NS);
    let current = sample_tf(16.0, END_NS);
    for (port, bridge_w) in [(BodyPort::Tf, 0.28), (BodyPort::Stage2, 0.30)] {
        let snapshot = compensated_snapshot(port, bridge_w, 0.2);
        let mut compensation = Compensation::new(&snapshot, CATALOG).unwrap().unwrap();
        compensation.accept(Some(previous));
        let evidence = compensation.evaluate(Some(current), END_NS + 1, BODY_AGE);
        let emulation = compensation_emulation(Some(evidence));
        let Some(EmulationMode::FixtureCompensation {
            capacity,
            temperature_rate,
            parasitic,
        }) = emulation
        else {
            panic!("expected fixture compensation");
        };
        assert!((capacity.get() - FIXTURE_CF).abs() < 1e-10);
        assert!((temperature_rate.get() + 0.1).abs() < 1e-12);
        assert_eq!(parasitic.get(), 0.05);
        let analytical = request(emulation.unwrap(), Watts::new(0.28).unwrap()).unwrap();
        assert!((analytical.requested.get() - EXPECTED_REQUEST).abs() < 1e-10);

        let mut body = VirtualBody::new(&snapshot, CATALOG.into()).unwrap();
        let held = body
            .sample(&tf_frame(16.0, MEASURED_NS), MEASURED_NS + 1, BODY_AGE)
            .unwrap();
        body.observe(held);
        let mut step = body.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
        let mut frame = tf_frame(16.0, END_NS);
        frame[1].read_started_ns = Some(END_NS);
        frame[1].read_finished_ns = Some(END_NS + 1);
        let current_port = body.sample(&frame, END_NS + 1, BODY_AGE);
        let (_, requested_w) = body
            .complete_tick(&mut step, current_port, None, emulation)
            .unwrap();
        assert!((step.q_v_w.unwrap() - bridge_w).abs() < 1e-10);
        assert!((requested_w.unwrap() - (EXPECTED_REQUEST + bridge_w - 0.28)).abs() < 1e-10);
    }
}

#[test]
fn missing_compensation_preserves_observed_flow_and_rejects_an_edit() {
    let snapshot = compensated_snapshot(BodyPort::Tf, 0.0, 0.2);
    let mut state = VirtualBody::new(&snapshot, CATALOG.into()).unwrap();
    state.observe(tf_at(&state, 16.0, MEASURED_NS));
    let edit = BodyEdit {
        mass_kg: Some(5.0),
        ..BodyEdit::default()
    };
    state.validate_edit(&edit).unwrap();
    let mut step = state.advance(BODY_STEP_NS, END_NS, BODY_AGE).unwrap();
    let before_j = state.enthalpy().get().to_bits();
    let current = tf_at(&state, 35.0, END_NS);
    let (_, request) = state
        .complete_tick(&mut step, Some(current), Some(edit), None)
        .unwrap();
    assert_eq!(request, None);
    let q_v = step.q_v_w.unwrap();
    assert!(q_v < 0.0);
    assert!((q_v - 0.02 * (step.temperature_k - 35.0)).abs() < 1e-12);
    assert_eq!(state.enthalpy().get().to_bits(), before_j);
    assert_eq!(step.change.unwrap().outcome, ModeChangeOutcomeV1::Rejected);
}

#[test]
fn fixture_compensation_requires_a_fresh_ordered_accepted_tf_pair() {
    let snapshot = compensated_snapshot(BodyPort::Stage2, 0.0, 0.2);
    let mut compensation = Compensation::new(&snapshot, CATALOG).unwrap().unwrap();
    let first = sample_tf(16.01, MEASURED_NS);
    let second = sample_tf(16.0, END_NS);
    let evaluate = |compensation: &mut Compensation, sample, now| {
        compensation_emulation(Some(compensation.evaluate(Some(sample), now, BODY_AGE)))
    };
    assert!(evaluate(&mut compensation, first, MEASURED_NS + 1).is_none());
    compensation.accept(Some(first));
    assert!(!compensation.ready(MEASURED_NS + 1, BODY_AGE));
    assert!(evaluate(&mut compensation, first, END_NS).is_none());
    assert!(evaluate(&mut compensation, second, MEASURED_NS + 200_000_001).is_none());
    assert!(evaluate(&mut compensation, second, END_NS + 1).is_some());
    assert!(!compensation.ready(END_NS + 1, BODY_AGE));
    compensation.accept(Some(second));
    assert!(compensation.ready(END_NS + 1, BODY_AGE));
    assert!(compensation.ready(MEASURED_NS + 200_000_000, BODY_AGE));
    assert!(!compensation.ready(MEASURED_NS + 200_000_001, BODY_AGE));
    compensation.accept(None);
    assert!(!compensation.ready(END_NS + 1, BODY_AGE));
    compensation.accept(Some(second));
    assert!(!compensation.ready(END_NS + 1, BODY_AGE));
    compensation.clear();
    assert!(!compensation.ready(END_NS + 1, BODY_AGE));
}

#[test]
fn compensation_head_applies_fixture_heat_and_rejects_unstable_steps() {
    let snapshot = compensated_snapshot(BodyPort::Tf, 0.0, 0.2);
    let mut compensation = Compensation::new(&snapshot, CATALOG).unwrap().unwrap();
    let mut head = snapshot.head.build().unwrap();
    let heater = [Watts::new(0.25).unwrap(), Watts::new(0.0).unwrap()];
    let cooling = snapshot
        .head
        .removal_w
        .map(|power| Watts::new(power).unwrap());
    compensation
        .advance_head(&mut head, Seconds::new(0.1).unwrap(), heater, cooling)
        .unwrap();
    let expected_tf = 16.0 + (0.25 + 0.05 - 0.5 * (16.0 - 15.0)) * 0.1 / 0.729_087_800_758;
    assert!((head.temperatures().tf.get() - expected_tf).abs() < 1e-10);
    let after = head.temperatures();
    assert!(compensation
        .advance_head(&mut head, Seconds::new(2.0).unwrap(), heater, cooling)
        .is_err());
    assert_eq!(head.temperatures(), after);

    let unstable = compensated_snapshot(BodyPort::Tf, 0.0, 0.001);
    assert!(Compensation::new(&unstable, CATALOG).is_err());
}
