use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_core::units::WattsPerKelvin;
use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::{
    record::{encode_line, Correlation, Demand, Event, Record},
    recovery::{recover, Tail},
    session::CloseResult,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Run(PathBuf);

enum Scenario {
    Nominal,
    SourceLoss,
    StaleTf,
}

impl Run {
    fn record(scenario: Scenario) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-a1-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self(root);
        let source = KeithleySimulator::start().unwrap();
        let lake = LakeShoreSim::spawn().unwrap();
        let mut owner = RecordingLoop::new_a1(
            &source,
            &lake,
            "a1-replay-fixture",
            fixture.path(),
            Instant::now(),
        )
        .unwrap();
        wait(|| owner.poll().ready);
        owner.recover().unwrap();
        start(&mut owner);
        tick(&mut owner).unwrap();
        match scenario {
            Scenario::SourceLoss => {
                source.disconnect_next_query();
                assert!(tick(&mut owner).is_err());
                assert_eq!(owner.output_state(), OutputState::Unknown);
                assert!(owner.fallback_requested());
                owner.recover().unwrap();
                assert_eq!(owner.output_state(), OutputState::OffVerified);
                owner.operate(OperatorAction::Reset).unwrap();
                assert!(owner.operate(OperatorAction::Start).is_err());
                start(&mut owner);
                assert!(tick(&mut owner).unwrap().command_latency.is_some());
            }
            Scenario::StaleTf => {
                thread::sleep(Duration::from_millis(1550));
                assert!(owner.tick(Instant::now()).is_err());
                assert_eq!(owner.output_state(), OutputState::OffVerified);
            }
            Scenario::Nominal => {
                owner.change_a1(Some(0.2), None).unwrap();
                tick(&mut owner).unwrap();
                owner.change_a1(None, WattsPerKelvin::new(0.04)).unwrap();
                for _ in 0..3 {
                    tick(&mut owner).unwrap();
                }
            }
        }
        owner.begin_close().unwrap();
        assert_eq!(owner.output_state(), OutputState::OffVerified);
        wait(|| matches!(owner.poll_close(), CloseResult::Completed(_)));
        fixture
    }

    fn path(&self) -> PathBuf {
        self.0.join("run")
    }

    fn replay(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
            .arg("replay")
            .arg(self.path())
            .output()
            .unwrap()
    }

    fn digests(&self) -> Vec<[u8; 32]> {
        [
            "config.snapshot.yaml",
            "meta.json",
            "raw.ndjson.zst",
            "events.ndjson",
            "references/nist_csv",
        ]
        .map(|name| Sha256::digest(fs::read(self.path().join(name)).unwrap()).into())
        .into()
    }

    fn alter(&self, mut change: impl FnMut(&mut Record<'_>) -> bool) -> u64 {
        let path = self.path().join("raw.ndjson.zst");
        let mut rows = Vec::new();
        let mut changed = None;
        let result = recover(fs::File::open(&path).unwrap(), |mut row| {
            if changed.is_none() && change(&mut row) {
                changed = Some(row.seq);
            }
            rows.push(encode_line(&row).unwrap());
        })
        .unwrap();
        assert_eq!(result.tail, Tail::FrameBoundary);
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.window_log(18).unwrap();
        encoder
            .set_pledged_src_size(Some(rows.iter().map(|row| row.len() as u64).sum()))
            .unwrap();
        for row in rows {
            encoder.write_all(&row).unwrap();
        }
        fs::write(path, encoder.finish().unwrap()).unwrap();
        changed.expect("fixture row to alter")
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
}

fn tick(
    owner: &mut RecordingLoop<'_>,
) -> Result<cvtl_sim::recording::TickReport, cvtl_sim::recording::Error> {
    let due = Instant::now() + PERIOD;
    thread::sleep(PERIOD);
    owner.tick(due)
}

fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        assert!(Instant::now() < deadline, "writer deadline");
        thread::park_timeout(Duration::from_millis(2));
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "CLI report missing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

#[test]
fn loop_recomputes_thermal_steps_and_control_without_writes() {
    let run = Run::record(Scenario::Nominal);
    let hashes = run.digests();
    let first = run.replay();
    let result = report(&first);
    assert!(first.status.success(), "{result}");
    assert_eq!(result["status"], "matched");
    assert_eq!(result["replayed_ticks"], 5);
    assert_eq!(result["a1"]["thermal_steps"], 5);
    assert!(result["a1"]["last_interval_end_ns"].as_u64().unwrap() > 0);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
    assert_eq!(result["historical_completion"], "unknown");
    assert_eq!(run.replay().stdout, first.stdout);
    assert_eq!(run.digests(), hashes);
}

#[test]
fn derived_fields_cannot_substitute_for_the_body_calculation() {
    let run = Run::record(Scenario::Nominal);
    let raw = run.path().join("raw.ndjson.zst");
    let original = fs::read(&raw).unwrap();
    for change_enthalpy in [true, false] {
        fs::write(&raw, &original).unwrap();
        let seq = run.alter(|row| {
            let Event::Inputs { frame } = &mut row.event else {
                return false;
            };
            if change_enthalpy {
                if let Some(dh) = frame
                    .a1_step_v1
                    .as_mut()
                    .and_then(|step| step.change.as_mut())
                    .and_then(|change| change.dh_event_j.as_mut())
                {
                    *dh += 1.0;
                    return true;
                }
            } else if let Demand::VirtualBody { requested_w } = &mut frame.demand {
                *requested_w += 0.1;
                return true;
            }
            false
        });
        let output = run.replay();
        assert!(!output.status.success());
        let result = report(&output);
        assert_eq!(result["status"], "mismatch", "{result}");
        assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
    }
}

#[test]
fn source_loss_and_manual_restart_require_causal_evidence() {
    let run = Run::record(Scenario::SourceLoss);
    let output = run.replay();
    let result = report(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["replayed_ticks"], 3);
    assert_eq!(result["a1"]["thermal_steps"], 3);
    assert_eq!(result["output_state_at_boundary"], "off_verified");

    let raw = run.path().join("raw.ndjson.zst");
    let original = fs::read(&raw).unwrap();
    // The fixture's second tick loses the source. Request 6 is the rejected Start.
    for operation in [Correlation::Tick(4), Correlation::Request(6)] {
        let seq = run.alter(|row| {
            if let Event::Dispatch {
                source,
                command,
                status,
                ..
            } = &mut row.event
            {
                if row.correlation == operation && *source == "lakeshore" {
                    *source = "keithley";
                    *command = "smua.source.output=1".into();
                    *status = "sent";
                    return true;
                }
            }
            false
        });
        let output = run.replay();
        assert!(!output.status.success());
        let result = report(&output);
        assert_eq!(result["status"], "mismatch", "{result}");
        assert_eq!(result["first_diagnostic"]["seq"], seq, "{result}");
        fs::write(&raw, &original).unwrap();
    }

    run.alter(|row| {
        if let Event::Response {
            source: "keithley",
            command,
            status,
            ..
        } = &mut row.event
        {
            if *status == "disconnected" && command == "print(smua.measure.iv())" {
                *status = "invalid_response";
                return true;
            }
        }
        false
    });
    let output = run.replay();
    assert!(!output.status.success());
    let result = report(&output);
    assert_eq!(result["status"], "insufficient", "{result}");
    assert_eq!(result["replayed_ticks"], 1);
}

#[test]
fn invalid_model_tick_replays_off_without_fabricating_a_thermal_step() {
    let run = Run::record(Scenario::StaleTf);
    let output = run.replay();
    let result = report(&output);
    assert!(output.status.success(), "{result}");
    assert_eq!(result["replayed_ticks"], 2);
    assert_eq!(result["a1"]["thermal_steps"], 1);
    assert_eq!(result["output_state_at_boundary"], "off_verified");
}

#[test]
fn catalog_profile_and_tail_diagnostics_are_separate() {
    let run = Run::record(Scenario::Nominal);
    let path = run.path();
    let catalog = path.join("references/nist_csv");
    let original_catalog = fs::read(&catalog).unwrap();
    let meta = path.join("meta.json");
    let original_meta = fs::read(&meta).unwrap();
    fs::remove_file(&catalog).unwrap();
    assert!(!run.replay().status.success());
    fs::write(&catalog, b"changed catalog").unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256"));
    let altered = b"ofhc_rrr100,cp,4,300,0,1,0,0,0,0,0,0,0,0\n";
    fs::write(&catalog, altered).unwrap();
    let mut header: Value = serde_json::from_slice(&original_meta).unwrap();
    header["references"][0]["sha256"] = format!("{:x}", Sha256::digest(altered)).into();
    fs::write(&meta, serde_json::to_vec(&header).unwrap()).unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert_eq!(report(&output)["status"], "mismatch");
    fs::write(&catalog, original_catalog).unwrap();

    let mut header: Value = serde_json::from_slice(&original_meta).unwrap();
    header["scenario"] = "unknown_a1_profile".into();
    fs::write(&meta, serde_json::to_vec(&header).unwrap()).unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("профиль"));
    fs::write(&meta, original_meta).unwrap();
    let raw = path.join("raw.ndjson.zst");
    let length = fs::metadata(&raw).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&raw)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let output = run.replay();
    assert!(!output.status.success());
    assert_eq!(report(&output)["raw_tail"], "incomplete_frame");
}
