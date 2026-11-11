use cvtl_store::record::{
    decode_line, encode_line, A1StepV1, Correlation, Demand, Event, ModeBodyChangeV1,
    ModeBodyStepV1, ModeChangeOutcomeV1, ModeCompensationIssue, ModeCompensationV1, ModeStepV1,
    Record, RecordError, Snapshot, ThermalPoint, MAX_RECORD_BYTES,
};
use serde_json::json;
use std::borrow::Cow;

// Extracted from the verified 600-tick v1 fixture before the extension.
const LEGACY_INPUTS: &[u8] = concat!(r#"{"schema":1,"run":"m6-14850-1794337250046742000","generation":1,"seq":146,"accepted_ns":19835541,"correlation":{"type":"request","id":1},"kind":"inputs","frame":{"scheduled_ns":18954000,"measured_ns":19832791,"wall_time_s":1794337250.066817,"dt_ns":0,"power":[{"channel":"a","quality":"measured","current_a":0.0,"voltage_v":0.0,"target_w":0.0,"read_started_ns":18954500,"read_finished_ns":19009875},{"channel":"b","quality":"measured","current_a":0.0,"voltage_v":0.0,"target_w":0.0,"read_started_ns":19011791,"read_finished_ns":19055041}],"temperatures":[{"point":"stage1","channel":1,"quality":"measured","temperature_k":42.0,"read_started_ns":19056583,"read_finished_ns":19287666},{"point":"stage2","channel":2,"quality":"measured","temperature_k":42.0,"read_started_ns":19287791,"read_finished_ns":19472000},{"point":"tf","channel":3,"quality":"measured","temperature_k":42.0,"read_started_ns":19472208,"read_finished_ns":19657541},{"point":"jacket","channel":4,"quality":"measured","temperature_k":42.0,"read_started_ns":19657750,"read_finished_ns":19832208}],"demand":{"mode":"power","requested_w":0.75},"source_connected":true,"thermometer_connected":true,"missed_tick":false,"allow_stale_once":false,"expect_tick":false,"queue_fault":false,"model_valid":true,"journal_ok":true,"cooling_phase":"cooling"}}"#, "\n").as_bytes();

fn record(event: Event<'_>) -> Record<'_> {
    Record::new("run-a", 3, 1, 42, Correlation::Tick(7), event)
}

#[test]
fn full_wire_record_round_trips_with_stable_tag_and_line_boundary() {
    let event = Event::Response {
        source: "keithley",
        command: Cow::Borrowed("print(\u{03a9})\n"),
        bytes: Some(Cow::Borrowed(&[0, 13, 10, 255])),
        status: "ok",
        at_ns: 50,
    };
    let expected = record(event);
    let line = encode_line(&expected).unwrap();
    assert_eq!(line.last(), Some(&b'\n'));
    assert!(!line[..line.len() - 1].contains(&b'\n'));
    assert!(std::str::from_utf8(&line)
        .unwrap()
        .contains("\"kind\":\"response\""));
    assert_eq!(decode_line(&line).unwrap(), expected);
}

#[test]
fn exact_record_limit_counts_utf8_escaping_and_lf() {
    let build = |body| {
        record(Event::Dispatch {
            source: "synthetic",
            command: Cow::Owned(body),
            status: "sent",
            at_ns: 50,
        })
    };
    let prefix = "µ\"\n";
    let overhead = encode_line(&build(prefix.into())).unwrap().len();
    let payload = format!("{prefix}{}", "x".repeat(MAX_RECORD_BYTES - overhead));
    let line = encode_line(&build(payload.clone())).unwrap();
    assert_eq!(line.len(), MAX_RECORD_BYTES);
    assert_eq!(decode_line(&line).unwrap().seq, 1);
    assert!(matches!(
        encode_line(&build(payload + "x")),
        Err(RecordError::Size)
    ));
}

#[test]
fn rejects_incompatible_or_invalid_records() {
    let line = encode_line(&record(Event::Operator {
        action: "arm",
        at_ns: 42,
    }))
    .unwrap();
    let text = std::str::from_utf8(&line).unwrap();
    assert!(matches!(
        decode_line(text.replace("\"schema\":1", "\"schema\":9").as_bytes()),
        Err(RecordError::Schema)
    ));
    assert!(matches!(
        decode_line(&line[..line.len() - 1]),
        Err(RecordError::Format)
    ));
    assert!(matches!(
        encode_line(&record(Event::Operator {
            action: "bad \"tag\"",
            at_ns: 0
        })),
        Err(RecordError::Format)
    ));
    let mut invalid = record(Event::Parameters {
        mass_kg: Some(2.0),
        material: None,
        bridge: None,
        dh_event_j: Some(f64::NAN),
        at_ns: 42,
    });
    assert!(matches!(encode_line(&invalid), Err(RecordError::Format)));
    invalid.seq = 0;
    assert!(matches!(encode_line(&invalid), Err(RecordError::Format)));
}

#[test]
fn snapshot_hashes_exact_applied_bytes_and_used_sources() {
    let snapshot = Snapshot {
        config_yaml: "hardware_enabled: false\n".into(),
        scenario: "test_only_recording".into(),
        references: vec![("example".into(), b"abc".to_vec())],
    };
    let meta: serde_json::Value =
        serde_json::from_slice(&snapshot.metadata("run-a", 3).unwrap()).unwrap();
    assert_eq!(meta["run"], "run-a");
    assert_eq!(meta["generation"], 3);
    assert_eq!(meta["scenario"], "test_only_recording");
    assert_eq!(
        meta["references"][0]["sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let mut changed = snapshot;
    changed.config_yaml.push(' ');
    assert_ne!(
        serde_json::from_slice::<serde_json::Value>(&changed.metadata("run-a", 3).unwrap())
            .unwrap()["config_sha256"],
        meta["config_sha256"],
    );
}

#[test]
fn floating_point_inputs_preserve_their_bits() {
    let value = f64::from_bits(0x3fe3_5d27_0ecb_82be);
    let row = record(Event::Parameters {
        mass_kg: Some(value),
        material: None,
        bridge: None,
        dh_event_j: Some(-0.0),
        at_ns: 0,
    });
    let bytes = encode_line(&row).unwrap();
    let Event::Parameters {
        mass_kg: Some(mass),
        dh_event_j: Some(energy),
        ..
    } = decode_line(&bytes).unwrap().event
    else {
        panic!("expected parameters");
    };
    assert_eq!(mass.to_bits(), value.to_bits());
    assert_eq!(energy.to_bits(), (-0.0_f64).to_bits());
}

#[test]
fn a1_step_round_trips_without_changing_legacy_inputs() {
    use cvtl_store::record::A1ChangeV1;
    let mut row = decode_line(LEGACY_INPUTS).unwrap();
    assert_eq!(encode_line(&row).unwrap(), LEGACY_INPUTS);
    let Event::Inputs { frame } = &mut row.event else {
        panic!("expected inputs");
    };
    assert!(frame.a1_step_v1.is_none());
    frame.demand = Demand::VirtualBody { requested_w: 0.28 };
    frame.a1_step_v1 = Some(A1StepV1 {
        interval_ns: 100_000_000,
        heater_energy_j: [0.02, 0.03],
        held_tf_k: 16.0,
        held_tf_age_ns: 101_000_000,
        body_temperature_k: 29.999,
        change: Some(A1ChangeV1 {
            mass_kg: None,
            scale: Some(0.2),
            bridge_w_per_k: None,
            dh_event_j: Some(3.0),
            temperature_before_k: 29.999,
            temperature_after_k: 29.999,
        }),
    });
    let bytes = encode_line(&row).unwrap();
    assert!(std::str::from_utf8(&bytes)
        .unwrap()
        .contains("\"a1_step_v1\""));
    assert!(!std::str::from_utf8(&bytes).unwrap().contains("\"mass_kg\""));
    assert_eq!(decode_line(&bytes).unwrap(), row);

    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    let change = frame.a1_step_v1.as_mut().unwrap().change.as_mut().unwrap();
    change.mass_kg = Some(10.0);
    change.scale = None;
    change.dh_event_j = Some(-2.0);
    let bytes = encode_line(&row).unwrap();
    assert_eq!(decode_line(&bytes).unwrap(), row);

    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    let change = frame.a1_step_v1.as_mut().unwrap().change.as_mut().unwrap();
    change.mass_kg = Some(0.0);
    assert!(matches!(encode_line(&row), Err(RecordError::Format)));
    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    let change = frame.a1_step_v1.as_mut().unwrap().change.as_mut().unwrap();
    change.mass_kg = Some(10.0);
    change.dh_event_j = None;
    assert!(matches!(encode_line(&row), Err(RecordError::Format)));
    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    frame.a1_step_v1.as_mut().unwrap().interval_ns = 0;
    frame
        .a1_step_v1
        .as_mut()
        .unwrap()
        .change
        .as_mut()
        .unwrap()
        .dh_event_j = Some(-2.0);
    assert!(matches!(encode_line(&row), Err(RecordError::Format)));
}

fn mode_input() -> Record<'static> {
    let mut row = decode_line(LEGACY_INPUTS).unwrap();
    row.correlation = Correlation::Tick(7);
    let Event::Inputs { frame } = &mut row.event else {
        panic!("expected inputs");
    };
    frame.demand = Demand::PowerProfile { requested_w: 1.0 };
    frame.mode_step_v1 = Some(ModeStepV1 {
        interval_ns: frame.measured_ns,
        interval_end_ns: frame.measured_ns,
        heater_energy_j: [0.0, f64::from_bits(0x3fa9_9999_9999_999b)],
        body: None,
    });
    row
}

#[test]
fn mode_step_round_trips_a_complete_interval() {
    let row = mode_input();
    let bytes = encode_line(&row).unwrap();
    let line = std::str::from_utf8(&bytes).unwrap();
    assert!(line.contains("\"mode_step_v1\""));
    assert!(!line.contains("\"body\""));
    assert_eq!(decode_line(&bytes).unwrap(), row);
}

fn mode_body_input(body: ModeBodyStepV1) -> Record<'static> {
    let mut row = mode_input();
    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    frame.demand = Demand::VirtualBody { requested_w: 0.0 };
    frame.mode_step_v1 = Some(ModeStepV1 {
        interval_ns: 10,
        interval_end_ns: 100,
        heater_energy_j: [0.0, 0.1],
        body: Some(body),
    });
    row
}

fn mode_body_step() -> ModeBodyStepV1 {
    ModeBodyStepV1 {
        port: ThermalPoint::Tf,
        held_k: 16.0,
        held_read_started_ns: 91,
        held_age_ns: 9,
        temperature_k: 30.0,
        q_v_w: Some(-0.25),
        change: None,
        compensation: None,
    }
}

#[test]
fn mode_body_step_round_trips_both_ports_and_change_outcomes() {
    let request = json!({"material": "Al6061", "bridge": {"kind": "constant", "g_w_per_k": 0.1}});
    for (port, outcome, status) in [
        (
            ThermalPoint::Tf,
            ModeChangeOutcomeV1::Applied {
                temperature_before_k: 30.0,
                temperature_after_k: 30.0,
                dh_event_j: Some(-0.5),
            },
            "applied",
        ),
        (
            ThermalPoint::Stage2,
            ModeChangeOutcomeV1::Rejected,
            "rejected",
        ),
    ] {
        let row = mode_body_input(ModeBodyStepV1 {
            port,
            change: Some(ModeBodyChangeV1 {
                request: request.clone(),
                outcome,
            }),
            ..mode_body_step()
        });
        let bytes = encode_line(&row).unwrap();
        assert_eq!(decode_line(&bytes).unwrap(), row);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let body = &value["frame"]["mode_step_v1"]["body"];
        assert!(body.get("compensation").is_none());
        let change = &body["change"];
        assert_eq!(change["request"], request);
        assert_eq!(change["outcome"]["status"], status);
    }
}

fn reject_mode_input(row: &Record<'_>) {
    assert_eq!(encode_line(row), Err(RecordError::Format));
    let mut unchecked_line = serde_json::to_vec(row).unwrap();
    unchecked_line.push(b'\n');
    assert_eq!(decode_line(&unchecked_line), Err(RecordError::Format));
}

fn compensation_input(compensation: ModeCompensationV1) -> Record<'static> {
    mode_body_input(ModeBodyStepV1 {
        compensation: Some(compensation),
        ..mode_body_step()
    })
}

const READY_COMPENSATION: ModeCompensationV1 = ModeCompensationV1::Ready {
    previous_tf_k: 16.001,
    previous_read_started_ns: 91,
    current_tf_k: 16.0,
    current_read_started_ns: 10_000_091,
    d_tf_k_per_s: -0.1,
    fixture_capacity_j_per_k: 0.729087800758,
    fixture_parasitic_w: 0.05,
};

#[test]
fn mode_compensation_round_trips_signed_evidence_and_unavailable_reasons() {
    for (evidence, reason) in [
        (READY_COMPENSATION, None),
        (
            ModeCompensationV1::Unavailable {
                reason: ModeCompensationIssue::MissingTf,
            },
            Some("missing_tf"),
        ),
        (
            ModeCompensationV1::Unavailable {
                reason: ModeCompensationIssue::PairNotReady,
            },
            Some("pair_not_ready"),
        ),
        (
            ModeCompensationV1::Unavailable {
                reason: ModeCompensationIssue::FixtureInvalid,
            },
            Some("fixture_invalid"),
        ),
    ] {
        let row = compensation_input(evidence);
        let bytes = encode_line(&row).unwrap();
        assert_eq!(decode_line(&bytes).unwrap(), row);
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let compensation = &wire["frame"]["mode_step_v1"]["body"]["compensation"];
        match reason {
            Some(reason) => assert_eq!(
                *compensation,
                json!({"status": "unavailable", "reason": reason}),
            ),
            None => assert_eq!(compensation["status"], "ready"),
        }
    }
}

#[test]
fn mode_compensation_rejects_impossible_bounds_and_quantities() {
    let bytes = encode_line(&compensation_input(READY_COMPENSATION)).unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (field, value) in [
        ("previous_read_started_ns", json!(10_000_091)),
        ("current_read_started_ns", json!(u64::MAX)),
        ("previous_tf_k", json!(0.0)),
        ("current_tf_k", json!(-1.0)),
        ("d_tf_k_per_s", json!(null)),
        ("fixture_capacity_j_per_k", json!(0.0)),
        ("fixture_parasitic_w", json!(-0.05)),
    ] {
        let mut invalid = valid.clone();
        invalid["frame"]["mode_step_v1"]["body"]["compensation"][field] = value;
        let mut line = serde_json::to_vec(&invalid).unwrap();
        line.push(b'\n');
        assert_eq!(decode_line(&line), Err(RecordError::Format), "{field}");
    }
}

#[test]
fn mode_body_step_rejects_invalid_raw_evidence() {
    for body in [
        ModeBodyStepV1 {
            port: ThermalPoint::Stage1,
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            held_read_started_ns: 101,
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            held_age_ns: 11,
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            held_k: 0.0,
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            temperature_k: f64::NAN,
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            change: Some(ModeBodyChangeV1 {
                request: json!("Al6061"),
                outcome: ModeChangeOutcomeV1::Rejected,
            }),
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            change: Some(ModeBodyChangeV1 {
                request: json!({"material": "Al6061"}),
                outcome: ModeChangeOutcomeV1::Applied {
                    temperature_before_k: 30.0,
                    temperature_after_k: -1.0,
                    dh_event_j: Some(-0.5),
                },
            }),
            ..mode_body_step()
        },
    ] {
        reject_mode_input(&mode_body_input(body));
    }
}

#[test]
fn mode_body_step_rejects_nonfinite_optional_numbers_before_encoding() {
    for body in [
        ModeBodyStepV1 {
            q_v_w: Some(f64::INFINITY),
            ..mode_body_step()
        },
        ModeBodyStepV1 {
            change: Some(ModeBodyChangeV1 {
                request: json!({"material": "Al6061"}),
                outcome: ModeChangeOutcomeV1::Applied {
                    temperature_before_k: 30.0,
                    temperature_after_k: 30.0,
                    dh_event_j: Some(f64::NAN),
                },
            }),
            ..mode_body_step()
        },
    ] {
        assert_eq!(
            encode_line(&mode_body_input(body)),
            Err(RecordError::Format)
        );
    }
}

#[test]
fn mode_step_rejects_invalid_interval_bounds_and_energy() {
    let row = mode_input();
    let Event::Inputs { frame } = &row.event else {
        unreachable!()
    };
    let valid = frame.mode_step_v1.clone().unwrap();
    for step in [
        ModeStepV1 {
            interval_ns: 0,
            ..valid.clone()
        },
        ModeStepV1 {
            interval_ns: valid.interval_end_ns + 1,
            ..valid.clone()
        },
        ModeStepV1 {
            interval_end_ns: frame.measured_ns + 1,
            ..valid.clone()
        },
        ModeStepV1 {
            heater_energy_j: [-0.1, 0.0],
            ..valid.clone()
        },
        ModeStepV1 {
            heater_energy_j: [0.0, f64::NAN],
            ..valid.clone()
        },
        ModeStepV1 {
            heater_energy_j: [f64::INFINITY, 0.0],
            ..valid
        },
    ] {
        let mut invalid = row.clone();
        let Event::Inputs { frame } = &mut invalid.event else {
            unreachable!()
        };
        frame.mode_step_v1 = Some(step);
        reject_mode_input(&invalid);
    }
}

#[test]
fn mode_step_cannot_coexist_with_a_legacy_a1_step() {
    let mut row = mode_input();
    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    let mode_step = frame.mode_step_v1.take();
    frame.demand = Demand::VirtualBody { requested_w: 0.28 };
    frame.a1_step_v1 = Some(A1StepV1 {
        interval_ns: 10_000_000,
        heater_energy_j: [0.02, 0.03],
        held_tf_k: 16.0,
        held_tf_age_ns: 10_000_000,
        body_temperature_k: 29.999,
        change: None,
    });
    assert!(encode_line(&row).is_ok());

    let Event::Inputs { frame } = &mut row.event else {
        unreachable!()
    };
    frame.mode_step_v1 = mode_step;
    reject_mode_input(&row);
}
