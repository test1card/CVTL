use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        atomic::{AtomicU64, Ordering},
        Barrier,
    },
    thread,
    time::{Duration, Instant},
};

use cvtl_cli::{check_archive, TimelineSample};
use cvtl_safety::{machine::OutputState, supervisor::OperatorAction};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::session::CloseResult;
use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::Field,
};
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
        for index in 0..3 {
            thread::sleep(PERIOD);
            assert!(owner
                .tick(Instant::now())
                .unwrap()
                .command_latency
                .is_some());
            if index == 0 {
                owner.change_a1(Some(0.2), None).unwrap();
            }
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

    fn primary_hashes(&self) -> BTreeMap<PathBuf, [u8; 32]> {
        let mut hashes = BTreeMap::new();
        for entry in fs::read_dir(&self.run).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "derived" || name == "report.html" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                assert_eq!(name, "references");
                for reference in fs::read_dir(entry.path()).unwrap() {
                    let reference = reference.unwrap();
                    hashes.insert(
                        PathBuf::from(&name).join(reference.file_name()),
                        digest(&reference.path()),
                    );
                }
            } else {
                hashes.insert(PathBuf::from(name), digest(&entry.path()));
            }
        }
        hashes
    }

    fn export(&self, options: &[&str]) -> Value {
        json_report(&self.run("export", options))
    }

    fn summary(&self, export: &Value) -> Value {
        let name = export["derived"].as_str().unwrap();
        let bytes = fs::read(self.run.join(name).join("summary.json")).unwrap();
        assert_eq!(name, format!("derived/{:x}", Sha256::digest(&bytes)));
        serde_json::from_slice(&bytes).unwrap()
    }

    fn publication(&self, export: &Value) -> [Vec<u8>; 4] {
        let derived = self.run.join(export["derived"].as_str().unwrap());
        let names = ["summary.json", "ticks.csv", "ticks.parquet"];
        let mut actual: Vec<_> = fs::read_dir(&derived)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        actual.sort();
        assert_eq!(actual, names);
        let [summary, csv, parquet] = names.map(|name| fs::read(derived.join(name)).unwrap());
        [
            fs::read(self.run.join("report.html")).unwrap(),
            summary,
            csv,
            parquet,
        ]
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

fn assert_export_failed(output: Output) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!output.stderr.is_empty(), "{output:?}");
}

fn tick_fields(sample: &TimelineSample) -> [(&'static str, Field); 28] {
    let [stage1, stage2, tf, jacket] = sample.temperatures_k;
    let [measured_a, measured_b] = sample.measured_w;
    let measured_total = measured_a.zip(measured_b).map(|(a, b)| a + b);
    let [target_a, target_b] = sample.targets_w.map_or([None; 2], |watts| watts.map(Some));
    let change = sample.change;
    let double = |value: Option<f64>| value.map_or(Field::Null, Field::Double);
    let flag = |value| {
        sample
            .equivalence
            .as_ref()
            .map_or(Field::Null, |_| Field::Bool(value))
    };
    [
        ("tick", Field::ULong(sample.tick)),
        ("seq", Field::ULong(sample.seq)),
        ("measured_ns", Field::ULong(sample.measured_ns)),
        ("stage1_k", double(stage1)),
        ("stage2_k", double(stage2)),
        ("tf_k", double(tf)),
        ("jacket_k", double(jacket)),
        ("body_k", double(sample.body_k)),
        ("inversion_delta_k", double(sample.inversion_delta_k)),
        ("measured_a_w", double(measured_a)),
        ("measured_b_w", double(measured_b)),
        ("measured_total_w", double(measured_total)),
        ("requested_w", double(sample.requested_w)),
        ("limited_w", double(sample.limited_w)),
        ("target_a_w", double(target_a)),
        ("target_b_w", double(target_b)),
        ("bridge_flow_w", double(sample.bridge_flow_w)),
        ("tracking_error_w", double(sample.tracking_error_w)),
        (
            "changed_mass_kg",
            double(change.and_then(|value| value.mass_kg)),
        ),
        (
            "changed_scale",
            double(change.and_then(|value| value.scale)),
        ),
        (
            "changed_bridge_w_per_k",
            double(change.and_then(|value| value.bridge_w_per_k)),
        ),
        (
            "dh_event_j",
            double(change.and_then(|value| value.dh_event_j)),
        ),
        (
            "temperature_before_change_k",
            double(change.map(|value| value.temperature_before_k)),
        ),
        (
            "temperature_after_change_k",
            double(change.map(|value| value.temperature_after_k)),
        ),
        ("slew_limited", flag(sample.slew_limited)),
        ("power_limited", flag(sample.power_limited)),
        ("current_limited", flag(sample.current_limited)),
        (
            "equivalence",
            sample.equivalence.clone().map_or(Field::Null, Field::Str),
        ),
    ]
}

fn assert_tables_match_replay(derived: &Path, samples: impl IntoIterator<Item = TimelineSample>) {
    let csv = fs::read_to_string(derived.join("ticks.csv")).unwrap();
    assert!(!csv.contains('\r'));
    assert!(csv.ends_with('\n'));
    let mut lines = csv.lines();
    let header: Vec<_> = lines.next().unwrap().split(',').collect();
    let reader =
        SerializedFileReader::new(File::open(derived.join("ticks.parquet")).unwrap()).unwrap();
    let mut rows = reader.get_row_iter(None).unwrap();
    for sample in samples {
        let row = rows.next().unwrap().unwrap();
        let csv_fields: Vec<_> = lines.next().unwrap().split(',').collect();
        let expected = tick_fields(&sample);
        let actual: Vec<_> = row.get_column_iter().collect();
        assert_eq!(csv_fields.len(), expected.len());
        assert_eq!(actual.len(), expected.len());
        assert_eq!(header, expected.each_ref().map(|(name, _)| *name));
        for (((column, value), text), (name, expected)) in
            actual.into_iter().zip(csv_fields).zip(&expected)
        {
            assert_eq!(column.as_str(), *name);
            assert_eq!(value, expected, "{column}");
            match value {
                Field::Null => assert!(text.is_empty(), "{column}"),
                Field::ULong(value) => assert_eq!(text.parse::<u64>().unwrap(), *value, "{column}"),
                Field::Double(value) => {
                    let Field::Double(expected) = expected else {
                        unreachable!();
                    };
                    assert_eq!(value.to_bits(), expected.to_bits(), "{column}");
                    assert_eq!(
                        text.parse::<f64>().unwrap().to_bits(),
                        value.to_bits(),
                        "{column}"
                    );
                }
                Field::Bool(value) => assert_eq!(text.parse::<bool>().unwrap(), *value, "{column}"),
                Field::Str(value) => assert_eq!(text, value, "{column}"),
                other => panic!("unexpected {column} type {other:?}"),
            }
        }
    }
    assert!(rows.next().is_none());
    assert!(lines.next().is_none());
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
    let primary = fixture.primary_hashes();
    let export = fixture.export(&[]);
    let summary = fixture.summary(&export);
    assert_eq!(export["status"], "partial");
    assert_eq!(export["rows"], 3);
    assert_eq!(
        summary["replay"]["output_state_at_boundary"],
        "off_verified"
    );
    assert_eq!(summary["completion"]["status"], "unconfirmed");
    assert_eq!(summary["analysis"], mismatched);
    assert_eq!(fixture.primary_hashes(), primary);

    fs::remove_file(path).unwrap();
    let missing = json_report(&fixture.run("analyze", &[]));
    assert_eq!(missing["status"], "не определяется");
    assert!(missing["reason"].as_str().unwrap().contains("квитанц"));
    let primary = fixture.primary_hashes();
    let export = fixture.export(&[]);
    let summary = fixture.summary(&export);
    assert_eq!(export["status"], "partial");
    assert_eq!(export["rows"], 3);
    assert_eq!(summary["completion"]["status"], "missing");
    assert_eq!(summary["analysis"], missing);
    assert_eq!(fixture.primary_hashes(), primary);

    assert!(!fixture.run("analyze", &["--window", "2"]).status.success());
    assert!(!fixture
        .run("analyze", &["--seed", "1", "--seed", "2"])
        .status
        .success());
}

#[test]
fn export_publishes_verified_tables_and_m8_analysis() {
    let fixture = Fixture::record();
    let group = r#"<script data-test="&'">fixture</script>"#;
    let snapshot = fixture.run.join("config.snapshot.yaml");
    let meta_path = fixture.run.join("meta.json");
    let mut config: yaml_serde::Value =
        yaml_serde::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    config["test_only"]["a1"]["m8"] =
        yaml_serde::to_value(json!({"other_w":0.0,"group":group})).unwrap();
    let config = yaml_serde::to_string(&config).unwrap();
    fs::write(&snapshot, &config).unwrap();
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["config_sha256"] = format!("{:x}", Sha256::digest(config.as_bytes())).into();
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let primary = fixture.primary_hashes();
    assert!(primary.contains_key(Path::new("completed.json")));
    assert!(primary.contains_key(Path::new("events.ndjson")));
    let options = ["--start", "0", "--window", "12", "--seed", "42"];
    let export = fixture.export(&options);
    assert_eq!(export["report"], "report.html");
    assert_eq!(export["rows"], 3);
    assert_eq!(export["status"], "complete");
    let derived_name = export["derived"].as_str().unwrap();
    let summary = fixture.summary(&export);
    assert_eq!(summary["rows"], 3);
    assert_eq!(summary["status"], "complete");
    assert_eq!(summary["mode"], "simulation");
    assert_eq!(summary["conditions"]["group"], group);
    assert_eq!(summary["raw_identity"], "matched");
    assert_eq!(summary["completion"]["status"], "verified");
    assert_eq!(
        summary["completion"]["sha256"],
        format!(
            "{:x}",
            Sha256::digest(fs::read(fixture.run.join("completed.json")).unwrap())
        )
    );
    assert_eq!(summary["replay"], json_report(&fixture.run("replay", &[])));
    assert_eq!(
        summary["analysis"],
        json_report(&fixture.run("analyze", &options))
    );
    assert_eq!(
        summary["analysis_options"],
        json!({"start_s":0.0,"window_s":12.0,"seed":42})
    );
    for (key, name) in [
        ("meta_sha256", "meta.json"),
        ("raw_sha256", "raw.ndjson.zst"),
    ] {
        let bytes = fs::read(fixture.run.join(name)).unwrap();
        assert_eq!(
            summary["source"][key],
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
    assert_eq!(
        summary["source"]["raw_bytes"],
        fs::metadata(fixture.run.join("raw.ndjson.zst"))
            .unwrap()
            .len()
    );
    assert_eq!(
        summary["source"]["meta"],
        serde_json::from_slice::<Value>(&fs::read(fixture.run.join("meta.json")).unwrap()).unwrap()
    );

    let replay = check_archive(&fixture.run).unwrap();
    assert_eq!(summary["last_verified_seq"], replay.last_verified_seq);
    assert_eq!(replay.samples.len(), 3);
    assert!(replay.samples.iter().any(|sample| sample.change.is_some()));
    assert_tables_match_replay(&fixture.run.join(derived_name), replay.samples);

    let published = fixture.publication(&export);
    let html = std::str::from_utf8(&published[0]).unwrap();
    assert!(!html.contains("<script"));
    assert!(
        html.contains(r#"&lt;script data-test=\&quot;&amp;&#39;\&quot;&gt;fixture&lt;/script&gt;"#)
    );
    for name in ["summary.json", "ticks.csv", "ticks.parquet"] {
        assert!(
            html.contains(&format!("{derived_name}/{name}")),
            "missing link to {name}"
        );
    }
    assert_eq!(fixture.export(&options), export);
    assert_eq!(fixture.publication(&export), published);
    assert_eq!(
        fs::read_dir(fixture.run.join("derived")).unwrap().count(),
        1
    );
    assert_export_failed(fixture.run("export", &["--pair", "missing"]));
    assert_eq!(fixture.publication(&export), published);
    assert_eq!(fixture.primary_hashes(), primary);
}

#[test]
fn export_failure_preserves_prior_publication() {
    let fixture = Fixture::record();
    let primary = fixture.primary_hashes();
    let first = fixture.export(&[]);
    let published = fixture.publication(&first);
    let other_options = ["--seed", "43"];
    let other = fixture.export(&other_options);
    assert_ne!(other["derived"], first["derived"]);
    assert_eq!(fixture.export(&[]), first);

    let conflict = fixture
        .run
        .join(other["derived"].as_str().unwrap())
        .join("extra.txt");
    fs::write(&conflict, b"occupied").unwrap();
    assert_export_failed(fixture.run("export", &other_options));
    assert_eq!(fs::read(conflict).unwrap(), b"occupied");
    assert_eq!(fixture.publication(&first), published);
    assert_eq!(
        fs::read_dir(fixture.run.join("derived")).unwrap().count(),
        2
    );
    assert_eq!(fixture.primary_hashes(), primary);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let receipt = fixture.run.join("completed.json");
        let permissions = fs::metadata(&receipt).unwrap().permissions();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o000)).unwrap();
        let export = File::open(&receipt).err().map(|error| {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            fixture.run("export", &[])
        });
        fs::set_permissions(receipt, permissions).unwrap();
        if let Some(export) = export {
            assert!(!export.status.success());
            assert!(
                String::from_utf8_lossy(&export.stderr).contains("Не удалось прочитать квитанцию")
            );
            assert_eq!(fixture.publication(&first), published);
            assert_eq!(fixture.primary_hashes(), primary);
        }
    }

    let snapshot = fixture.run.join("config.snapshot.yaml");
    let mut bytes = fs::read(&snapshot).unwrap();
    bytes.extend_from_slice(b"\n# SHA-256 mismatch\n");
    fs::write(&snapshot, bytes).unwrap();
    let modified = fixture.primary_hashes();
    assert_export_failed(fixture.run("export", &[]));
    assert_eq!(fixture.publication(&first), published);
    assert_eq!(
        fs::read_dir(fixture.run.join("derived")).unwrap().count(),
        2
    );
    assert_eq!(fixture.primary_hashes(), modified);
}

#[cfg(unix)]
#[test]
fn export_checks_destinations_without_truncating_primary() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::record();
    let primary = fixture.primary_hashes();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, b"outside").unwrap();
    let derived = fixture.run.join("derived");
    let report = fixture.run.join("report.html");

    for (target, link) in [(&outside, &derived), (&sentinel, &report)] {
        symlink(target, link).unwrap();
        assert_export_failed(fixture.run("export", &[]));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        fs::remove_file(link).unwrap();
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(fs::read(&sentinel).unwrap(), b"outside");
    }

    fs::write(&derived, b"occupied").unwrap();
    assert_export_failed(fixture.run("export", &[]));
    assert_eq!(fs::read(&derived).unwrap(), b"occupied");
    fs::remove_file(&derived).unwrap();
    fs::create_dir(&report).unwrap();
    assert_export_failed(fixture.run("export", &[]));
    assert!(report.is_dir());
    fs::remove_dir(&report).unwrap();

    fs::hard_link(fixture.run.join("raw.ndjson.zst"), &report).unwrap();
    assert_eq!(digest(&report), primary[Path::new("raw.ndjson.zst")]);
    let export = fixture.export(&[]);
    let html = fs::read_to_string(report).unwrap();
    assert!(html.contains(export["derived"].as_str().unwrap()));
    assert_eq!(fixture.primary_hashes(), primary);
}

#[test]
fn concurrent_exports_publish_complete_distinct_sets() {
    let fixture = Fixture::record();
    let primary = fixture.primary_hashes();
    let start = Barrier::new(2);
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            fixture.export(&["--seed", "42"])
        });
        let second = scope.spawn(|| {
            start.wait();
            fixture.export(&["--seed", "43"])
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(first["derived"], second["derived"]);
    let html = fs::read_to_string(fixture.run.join("report.html")).unwrap();
    let mut referenced = 0;
    for (export, seed) in [(first, 42), (second, 43)] {
        assert_eq!(export["status"], "complete");
        assert_eq!(fixture.summary(&export)["analysis"]["seed"], seed);
        let _ = fixture.publication(&export);
        if html.contains(export["derived"].as_str().unwrap()) {
            referenced += 1;
        }
    }
    assert_eq!(referenced, 1);
    assert_eq!(
        fs::read_dir(fixture.run.join("derived")).unwrap().count(),
        2
    );
    assert_eq!(fixture.primary_hashes(), primary);
}
