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
