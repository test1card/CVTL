use cvtl_store::record::{
    decode_line, encode_line, Correlation, Event, Record, RecordError, Snapshot, MAX_RECORD_BYTES,
};
use std::borrow::Cow;

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
    use cvtl_store::record::{A1ChangeV1, A1StepV1, Demand};
    // Extracted from the verified 600-tick v1 fixture before the extension.
    let legacy = concat!(r#"{"schema":1,"run":"m6-14850-1794337250046742000","generation":1,"seq":146,"accepted_ns":19835541,"correlation":{"type":"request","id":1},"kind":"inputs","frame":{"scheduled_ns":18954000,"measured_ns":19832791,"wall_time_s":1794337250.066817,"dt_ns":0,"power":[{"channel":"a","quality":"measured","current_a":0.0,"voltage_v":0.0,"target_w":0.0,"read_started_ns":18954500,"read_finished_ns":19009875},{"channel":"b","quality":"measured","current_a":0.0,"voltage_v":0.0,"target_w":0.0,"read_started_ns":19011791,"read_finished_ns":19055041}],"temperatures":[{"point":"stage1","channel":1,"quality":"measured","temperature_k":42.0,"read_started_ns":19056583,"read_finished_ns":19287666},{"point":"stage2","channel":2,"quality":"measured","temperature_k":42.0,"read_started_ns":19287791,"read_finished_ns":19472000},{"point":"tf","channel":3,"quality":"measured","temperature_k":42.0,"read_started_ns":19472208,"read_finished_ns":19657541},{"point":"jacket","channel":4,"quality":"measured","temperature_k":42.0,"read_started_ns":19657750,"read_finished_ns":19832208}],"demand":{"mode":"power","requested_w":0.75},"source_connected":true,"thermometer_connected":true,"missed_tick":false,"allow_stale_once":false,"expect_tick":false,"queue_fault":false,"model_valid":true,"journal_ok":true,"cooling_phase":"cooling"}}"#, "\n").as_bytes();
    let mut row = decode_line(legacy).unwrap();
    assert_eq!(encode_line(&row).unwrap(), legacy);
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
