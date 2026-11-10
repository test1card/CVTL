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
fn mismatched_run_or_generation_cannot_replay_raw() {
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
