use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::session::CloseResult;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

struct Fixture {
    root: PathBuf,
    run: PathBuf,
}

impl Fixture {
    fn record() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-m8-test-{}-{}",
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
        let mut owner = RecordingLoop::new_a1(
            &source,
            &lake,
            "analysis-fixture",
            fixture.run.clone(),
            epoch,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !owner.poll().ready {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        owner.recover().unwrap();
        owner.operate(OperatorAction::Arm).unwrap();
        owner.operate(OperatorAction::Start).unwrap();
        for _ in 0..3 {
            thread::sleep(PERIOD);
            assert!(owner
                .tick(Instant::now())
                .unwrap()
                .command_latency
                .is_some());
        }
        owner.begin_close().unwrap();
        assert_eq!(owner.output_state(), OutputState::OffVerified);
        let deadline = Instant::now() + Duration::from_secs(3);
        let completed = loop {
            match owner.poll_close() {
                CloseResult::Pending => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(2));
                }
                CloseResult::Completed(completed) => break completed,
                other => panic!("{other:?}"),
            }
        };
        fs::write(
            fixture.run.join("completed.json"),
            serde_json::to_vec(&json!({
                "receipt": {
                    "run":"analysis-fixture",
                    "generation":completed.generation,
                    "last_seq":completed.last_seq,
                    "output_state":"off_verified"
                },
                "diagnostics":{"wiring":[],"fixture":[]}
            }))
            .unwrap(),
        )
        .unwrap();
        fixture
    }

    fn run(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
            .arg(command)
            .arg(&self.run)
            .args(args)
            .output()
            .unwrap()
    }

    fn primary_hashes(&self) -> [[u8; 32]; 4] {
        [
            "meta.json",
            "config.snapshot.yaml",
            "raw.ndjson.zst",
            "references/nist_csv",
        ]
        .map(|file| digest(&self.run.join(file)))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn digest(path: &Path) -> [u8; 32] {
    Sha256::digest(fs::read(path).unwrap()).into()
}

fn json_report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn closed_short_run_reports_insufficiency_without_touching_archive() {
    let fixture = Fixture::record();
    let hashes = fixture.primary_hashes();
    let old = fixture.run("replay", &[]);
    assert_eq!(json_report(&old)["status"], "matched");
    let analysis = fixture.run(
        "analyze",
        &["--start", "0", "--window", "12", "--seed", "42"],
    );
    let report = json_report(&analysis);
    assert_eq!(report["status"], "не определяется");
    assert_eq!(report["seed"], 42);
    assert!(report["q2_eff_w"].is_null());
    assert!(report["reason"].as_str().unwrap().contains("окн"));
    assert_eq!(
        analysis.stdout,
        fixture
            .run(
                "analyze",
                &["--start", "0", "--window", "12", "--seed", "42"]
            )
            .stdout
    );
    assert_eq!(old.stdout, fixture.run("replay", &[]).stdout);
    assert_eq!(hashes, fixture.primary_hashes());
}

#[test]
fn missing_or_mismatched_completion_cannot_prove_analysis_ready() {
    let fixture = Fixture::record();
    let path = fixture.run.join("completed.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    receipt["receipt"]["last_seq"] = json!(999999);
    fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let mismatched = json_report(&fixture.run("analyze", &[]));
    assert_eq!(mismatched["status"], "не определяется");
    assert!(mismatched["reason"].as_str().unwrap().contains("квитанц"));
    fs::remove_file(path).unwrap();
    let missing = json_report(&fixture.run("analyze", &[]));
    assert_eq!(missing["status"], "не определяется");
    assert!(missing["reason"].as_str().unwrap().contains("квитанц"));
    assert!(!fixture.run("analyze", &["--window", "2"]).status.success());
    assert!(!fixture
        .run("analyze", &["--seed", "1", "--seed", "2"])
        .status
        .success());
}
