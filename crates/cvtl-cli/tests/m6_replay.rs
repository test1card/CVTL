use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_safety::{supervisor::OperatorAction, FaultReason};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::{Fault, LakeShoreSim},
    recording::RecordingLoop,
};
use cvtl_store::{
    record::{decode_line, encode_line, Correlation, Event},
    recovery::{recover, Tail},
    session::CloseResult,
    writer::spawn_file,
};
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::RowAccessor,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Fixture {
    root: PathBuf,
    run: PathBuf,
}
impl Fixture {
    fn record(thermometer_fault: bool, reconnect: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-m6-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            run: root.join("run"),
            root,
        };
        let source = KeithleySimulator::start().unwrap();
        let lake = LakeShoreSim::spawn().unwrap();
        let epoch = Instant::now();
        let session = spawn_file(
            "fixture",
            1,
            fixture.run.clone(),
            RecordingLoop::snapshot(),
            move || epoch.elapsed(),
        )
        .unwrap();
        let mut owner = RecordingLoop::new(&source, &lake, session, epoch);
        wait(|| owner.poll().ready);
        owner.recover().unwrap();
        start(&mut owner);
        owner.tick(Instant::now()).unwrap();
        if thermometer_fault {
            lake.set_fault(Fault::Disconnect);
            assert!(owner.tick(Instant::now()).is_err());
            assert_eq!(owner.fault_reason(), Some(FaultReason::ThermometerLinkLost));
        } else if reconnect {
            owner.recover().unwrap();
            start(&mut owner);
            assert!(owner.tick(Instant::now()).is_err());
            assert_eq!(owner.fault_reason(), Some(FaultReason::MonotonicClock));
        } else {
            owner.tick(Instant::now()).unwrap();
            owner.tick(Instant::now()).unwrap();
        }
        owner.begin_close().unwrap();
        wait(|| matches!(owner.poll_close(), CloseResult::Completed(_)));
        fixture
    }

    fn replay(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
            .arg("replay")
            .arg(&self.run)
            .output()
            .unwrap()
    }

    fn export(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
            .arg("export")
            .arg(&self.run)
            .output()
            .unwrap()
    }

    fn digest(&self) -> Vec<String> {
        [
            "config.snapshot.yaml",
            "meta.json",
            "raw.ndjson.zst",
            "events.ndjson",
        ]
        .into_iter()
        .map(|name| {
            format!(
                "{:x}",
                Sha256::digest(fs::read(self.run.join(name)).unwrap())
            )
        })
        .collect()
    }

    fn lines(&self) -> Vec<Vec<u8>> {
        let mut rows = Vec::new();
        let report = recover(
            fs::File::open(self.run.join("raw.ndjson.zst")).unwrap(),
            |row| {
                rows.push(encode_line(&row).unwrap());
            },
        )
        .unwrap();
        assert_eq!(report.tail, Tail::FrameBoundary);
        rows
    }

    fn write_raw(&self, bytes: &[u8]) {
        fs::write(self.run.join("raw.ndjson.zst"), bytes).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture writer deadline");
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "CLI did not produce a report: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn frame(lines: &[Vec<u8>]) -> Vec<u8> {
    let bytes: usize = lines.iter().map(Vec::len).sum();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.window_log(18).unwrap();
    encoder.set_pledged_src_size(Some(bytes as u64)).unwrap();
    for line in lines {
        encoder.write_all(line).unwrap();
    }
    encoder.finish().unwrap()
}

fn tick_inputs(lines: &[Vec<u8>]) -> Vec<(u64, u64)> {
    lines
        .iter()
        .filter_map(|line| {
            let row = decode_line(line).unwrap();
            match (row.correlation, row.event) {
                (Correlation::Tick(tick), Event::Inputs { .. }) => Some((tick, row.seq)),
                _ => None,
            }
        })
        .collect()
}

fn assert_m6_export(
    fixture: &Fixture,
    ticks: &[(u64, u64)],
    publication_status: &str,
    completion_status: &str,
) -> Value {
    let primary = fixture.digest();
    let raw_sha256 = format!(
        "{:x}",
        Sha256::digest(fs::read(fixture.run.join("raw.ndjson.zst")).unwrap())
    );
    let output = fixture.export();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let publication = json(&output);
    assert_eq!(publication["report"], "report.html");
    assert_eq!(publication["status"], publication_status);
    assert_eq!(publication["rows"], ticks.len());
    let derived = fixture.run.join(publication["derived"].as_str().unwrap());
    let summary: Value =
        serde_json::from_slice(&fs::read(derived.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["status"], publication_status);
    assert_eq!(summary["rows"], ticks.len());
    assert_eq!(summary["replay"], json(&fixture.replay()));
    assert_eq!(summary["source"]["raw_sha256"], raw_sha256);
    assert_eq!(summary["completion"]["status"], completion_status);
    assert_eq!(summary["analysis"]["status"], "не определяется");
    assert_eq!(primary, fixture.digest());

    let csv = fs::read_to_string(derived.join("ticks.csv")).unwrap();
    let mut rows = csv.lines();
    assert!(rows.next().unwrap().starts_with("tick,seq,"));
    let csv_ticks: Vec<(u64, u64)> = rows
        .map(|row| {
            let mut columns = row.split(',');
            (
                columns.next().unwrap().parse().unwrap(),
                columns.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    assert_eq!(csv_ticks, ticks);

    let parquet =
        SerializedFileReader::new(fs::File::open(derived.join("ticks.parquet")).unwrap()).unwrap();
    let parquet_ticks: Vec<_> = parquet
        .get_row_iter(None)
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            (row.get_ulong(0).unwrap(), row.get_ulong(1).unwrap())
        })
        .collect();
    assert_eq!(parquet_ticks, ticks);
    summary
}

#[test]
fn real_nominal_recording_recomputes_identically_without_writing() {
    let fixture = Fixture::record(false, false);
    let hashes = fixture.digest();
    let first = fixture.replay();
    assert!(first.status.success(), "{:?}", json(&first));
    let result = json(&first);
    assert_eq!(result["status"], "matched");
    assert_eq!(result["replayed_ticks"], 3);
    assert_eq!(result["historical_completion"], "unknown");
    assert_eq!(result["output_state_at_boundary"], "off_verified");
    assert_eq!(first.stdout, fixture.replay().stdout);
    assert_eq!(hashes, fixture.digest());
}

#[test]
fn export_legacy_run_without_receipt_keeps_completion_partial() {
    let fixture = Fixture::record(false, false);
    let ticks = tick_inputs(&fixture.lines());
    assert_eq!(ticks.len(), 3);
    let summary = assert_m6_export(&fixture, &ticks, "partial", "missing");
    assert_eq!(summary["replay"]["status"], "matched");
    assert_eq!(
        summary["replay"]["output_state_at_boundary"],
        "off_verified"
    );
    assert_eq!(summary["raw_identity"], "matched");
    assert_eq!(
        summary["last_verified_seq"],
        summary["replay"]["raw_verified_records"]
    );
}

#[test]
fn export_legacy_run_can_complete_without_a_cooling_estimate() {
    let fixture = Fixture::record(false, false);
    let replay = json(&fixture.replay());
    let receipt = serde_json::json!({"receipt": {
        "run": replay["run"],
        "generation": replay["generation"],
        "last_seq": replay["raw_verified_records"],
        "output_state": replay["output_state_at_boundary"],
    }});
    let receipt_path = fixture.run.join("completed.json");
    let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
    fs::write(&receipt_path, &receipt_bytes).unwrap();
    let ticks = tick_inputs(&fixture.lines());
    let summary = assert_m6_export(&fixture, &ticks, "complete", "verified");
    assert_eq!(
        summary["analysis"]["reason"],
        "Прогон не содержит тепловых интервалов A1"
    );
    assert_eq!(fs::read(receipt_path).unwrap(), receipt_bytes);
}

#[test]
fn export_tables_stop_at_the_last_semantically_verified_tick() {
    let fixture = Fixture::record(false, false);
    let mut lines = fixture.lines();
    let ticks = tick_inputs(&lines);
    assert_eq!(ticks.len(), 3);
    let start_of = |tick| {
        lines
            .iter()
            .position(|line| decode_line(line).unwrap().correlation == Correlation::Tick(tick))
            .unwrap()
    };
    let prefix_end = start_of(ticks[1].0);
    let damaged_end = start_of(ticks[2].0);
    let last_verified_seq = decode_line(&lines[prefix_end - 1]).unwrap().seq;
    let prefix = frame(&lines[..prefix_end]);
    let second = frame(&lines[prefix_end..damaged_end]);
    let trailing = frame(&lines[damaged_end..]);

    let mut corrupted = second.clone();
    *corrupted.last_mut().unwrap() ^= 1;
    let mut corrupted_raw = [prefix.as_slice(), &corrupted, &trailing].concat();
    // Keep some bytes beyond recover's buffered read to exercise full-file hashing.
    corrupted_raw.resize(corrupted_raw.len() + 64 * 1024, 0xa5);
    for (tail, raw) in [
        (
            "incomplete_frame",
            [prefix.as_slice(), &second[..second.len() - 2]].concat(),
        ),
        ("corrupt_frame", corrupted_raw),
    ] {
        fixture.write_raw(&raw);
        let summary = assert_m6_export(&fixture, &ticks[..1], "partial", "missing");
        assert_eq!(summary["replay"]["raw_tail"], tail);
        assert_eq!(summary["replay"]["status"], tail);
        assert_eq!(summary["replay"]["raw_verified_records"], last_verified_seq);
        assert_eq!(summary["last_verified_seq"], last_verified_seq);
        assert_eq!(summary["raw_identity"], "matched");
    }

    let changed_seq = lines[prefix_end..damaged_end]
        .iter_mut()
        .find_map(|line| {
            let mut row = decode_line(line).unwrap();
            let Event::Control {
                limited_w: Some(value),
                ..
            } = &mut row.event
            else {
                return None;
            };
            *value += 0.125;
            let seq = row.seq;
            *line = encode_line(&row).unwrap();
            Some(seq)
        })
        .unwrap();
    fixture.write_raw(&frame(&lines));
    let summary = assert_m6_export(&fixture, &ticks[..1], "partial", "missing");
    assert_eq!(summary["replay"]["status"], "mismatch");
    assert_eq!(summary["replay"]["raw_tail"], "frame_boundary");
    assert_eq!(summary["replay"]["raw_verified_records"], lines.len());
    assert_eq!(summary["replay"]["first_diagnostic"]["seq"], changed_seq);
    assert_eq!(summary["last_verified_seq"], last_verified_seq);
}

#[test]
fn export_empty_raw_does_not_confirm_identity() {
    let fixture = Fixture::record(false, false);
    fixture.write_raw(&[]);
    let summary = assert_m6_export(&fixture, &[], "partial", "missing");
    assert_eq!(summary["raw_identity"], "unverified");
    assert_eq!(summary["replay"]["raw_verified_records"], 0);
    assert_eq!(summary["last_verified_seq"], 0);
}

#[test]
fn fully_recorded_thermometer_failure_recomputes_m5_and_off() {
    let fixture = Fixture::record(true, false);
    let out = fixture.replay();
    assert!(out.status.success(), "{:?}", json(&out));
    let result = json(&out);
    assert_eq!(result["status"], "matched");
    assert_eq!(result["replayed_ticks"], 2);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
}

#[test]
fn consecutive_run_records_handle_stop_then_recover_in_order() {
    let fixture = Fixture::record(false, true);
    let out = fixture.replay();
    assert!(out.status.success(), "{:?}", json(&out));
    assert_eq!(json(&out)["replayed_ticks"], 2);
}

#[test]
fn altered_recorded_control_is_detected_in_a_valid_frame() {
    let fixture = Fixture::record(false, false);
    let mut rows = fixture.lines();
    let mut changed = None;
    for line in &mut rows {
        let mut row = decode_line(line).unwrap();
        if let Event::Control {
            limited_w: Some(value),
            ..
        } = &mut row.event
        {
            *value += 0.125;
            changed = Some(row.seq);
            *line = encode_line(&row).unwrap();
            break;
        }
    }
    fixture.write_raw(&frame(&rows));
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["status"], "mismatch");
    assert_eq!(report["first_diagnostic"]["seq"], changed.unwrap());
    assert_eq!(report["replayed_ticks"], 0);
}

#[test]
fn tail_corruption_and_complete_frame_with_partial_tick_have_distinct_results() {
    let fixture = Fixture::record(false, false);
    let rows = fixture.lines();
    let first_tick = rows
        .iter()
        .position(|line| matches!(decode_line(line).unwrap().correlation, Correlation::Tick(_)))
        .unwrap();
    let second_tick = rows
        .iter()
        .enumerate()
        .skip(first_tick + 1)
        .find_map(|(index, line)| {
            (decode_line(line).unwrap().correlation
                != decode_line(&rows[first_tick]).unwrap().correlation)
                .then_some(index)
        })
        .unwrap();
    let initial = frame(&rows[..first_tick]);
    let first = frame(&rows[first_tick..second_tick]);
    let rest = frame(&rows[second_tick..]);

    let mut damaged = first.clone();
    *damaged.last_mut().unwrap() ^= 1;
    fixture.write_raw(&[initial.clone(), damaged, rest.clone()].concat());
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["raw_tail"], "corrupt_frame");
    assert_eq!(report["raw_verified_records"], first_tick);
    assert_eq!(report["replayed_ticks"], 0);

    fixture.write_raw(&[initial, first, rest[..rest.len() - 2].to_vec()].concat());
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["raw_tail"], "incomplete_frame");
    assert_eq!(report["replayed_ticks"], 1);

    let last_input = rows
        .iter()
        .enumerate()
        .skip(second_tick)
        .find_map(|(index, line)| {
            matches!(decode_line(line).unwrap().event, Event::Inputs { .. }).then_some(index)
        })
        .unwrap();
    fixture.write_raw(&frame(&rows[..=last_input]));
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["raw_tail"], "frame_boundary");
    assert_eq!(report["status"], "insufficient");
    assert_eq!(report["replayed_ticks"], 1);
}

#[test]
fn missing_off_readback_does_not_accept_output_summary() {
    let fixture = Fixture::record(true, false);
    let mut rows = fixture.lines();
    let mut removed = false;
    for line in &mut rows {
        let mut row = decode_line(line).unwrap();
        if let Event::Response {
            command,
            bytes,
            status,
            ..
        } = &mut row.event
        {
            if matches!(row.correlation, Correlation::Tick(_))
                && command == "print(smub.source.output)"
            {
                *bytes = None;
                *status = "timeout";
                *line = encode_line(&row).unwrap();
                removed = true;
                break;
            }
        }
    }
    assert!(removed);
    fixture.write_raw(&frame(&rows));
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_eq!(report["status"], "insufficient");
    assert_eq!(report["replayed_ticks"], 1);
    assert_ne!(report["output_state_at_boundary"], "off_verified");
}

#[test]
fn mismatched_run_or_generation_cannot_replay_or_export_raw() {
    for (key, value) in [
        ("run", Value::from("another")),
        ("generation", Value::from(2)),
    ] {
        let fixture = Fixture::record(false, false);
        let path = fixture.run.join("meta.json");
        let mut meta: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        meta[key] = value;
        fs::write(path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let out = fixture.replay();
        assert!(!out.status.success());
        assert_eq!(json(&out)["status"], "mismatch");

        let primary = fixture.digest();
        let out = fixture.export();
        assert_eq!(out.status.code(), Some(1));
        assert!(!out.stderr.is_empty());
        assert!(!fixture.run.join("report.html").exists());
        assert_eq!(fixture.digest(), primary);
    }
}

#[test]
fn unknown_schema_refused_and_calculation_uses_snapshot_values() {
    let fixture = Fixture::record(false, false);
    let meta_path = fixture.run.join("meta.json");
    let cfg_path = fixture.run.join("config.snapshot.yaml");
    let mut cfg = String::new();
    fs::File::open(&cfg_path)
        .unwrap()
        .read_to_string(&mut cfg)
        .unwrap();
    let updated = cfg.replace("total_limit_w: 6,", "total_limit_w: 0.2,");
    assert_ne!(cfg, updated);
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["config_sha256"] = Value::from(format!("{:x}", Sha256::digest(updated.as_bytes())));
    fs::write(&cfg_path, updated).unwrap();
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let out = fixture.replay();
    assert!(!out.status.success());
    assert_eq!(json(&out)["status"], "mismatch");

    meta["schema"] = Value::from(9);
    fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let out = fixture.replay();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Несовместимая схема"));
}

#[test]
fn unknown_energizing_attempt_after_off_keeps_outcome_unproven() {
    use cvtl_store::record::Record;
    use std::borrow::Cow;

    let fixture = Fixture::record(false, false);
    let mut rows = fixture.lines();
    let last = rows.pop().unwrap();
    let mut summary = decode_line(&last).unwrap();
    assert!(matches!(
        summary.event,
        Event::OutputEvidence {
            state: "off_verified",
            ..
        }
    ));
    let unknown = Record::new(
        summary.run,
        summary.generation,
        summary.seq,
        summary.accepted_ns,
        Correlation::Run,
        Event::Dispatch {
            source: "keithley",
            command: Cow::Borrowed("smua.source.output = smua.OUTPUT_ON"),
            status: "unknown",
            at_ns: summary.accepted_ns,
        },
    );
    rows.push(encode_line(&unknown).unwrap());
    summary.seq += 1;
    rows.push(encode_line(&summary).unwrap());
    fixture.write_raw(&frame(&rows));
    let out = fixture.replay();
    assert!(!out.status.success());
    let report = json(&out);
    assert_ne!(report["status"], "matched");
    assert_ne!(report["output_state_at_boundary"], "off_verified");
}
