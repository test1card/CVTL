use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use cvtl_cli::TimelineSample;
use cvtl_store::{
    record::{Channel, Command as Intent, Correlation, Event},
    recovery::{recover, Recovery, Tail},
};
use serde::Serialize;
use serde_json::{json, Value};

struct Daemon {
    child: Child,
    root: PathBuf,
    host: String,
}

impl Daemon {
    fn start(test_faults: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cvtl-operator-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let assets = root.join("web");
        fs::create_dir_all(assets.join("assets")).unwrap();
        fs::write(assets.join("index.html"), b"<title>CVTL</title>").unwrap();
        fs::write(assets.join("assets/operator.js"), b"export {};").unwrap();
        let child = Self::spawn(&root, test_faults);
        let mut daemon = Self {
            child,
            root,
            host: String::new(),
        };
        daemon.read_host();
        daemon
    }

    fn spawn(root: &Path, test_faults: bool) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cvtl-daemon"));
        command
            .arg("serve")
            .arg("--assets")
            .arg(root.join("web"))
            .arg("--runs")
            .arg(root.join("runs"))
            .args(["--port", "0"]);
        if test_faults {
            command.arg("--test-faults");
        }
        command.stdout(Stdio::piped()).spawn().unwrap()
    }

    fn read_host(&mut self) {
        let mut line = String::new();
        BufReader::new(self.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        self.host = line
            .trim()
            .strip_prefix("http://")
            .expect("daemon localhost URL")
            .to_owned();
        assert!(self.host.starts_with("127.0.0.1:"));
    }

    fn stream(&self) -> TcpStream {
        let stream = TcpStream::connect(&self.host).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
    }

    fn raw(&self, request: &str) -> (u16, Vec<u8>) {
        let mut stream = self.stream();
        stream.write_all(request.as_bytes()).unwrap();
        read_response(stream)
    }

    fn get(&self, path: &str) -> Value {
        let (status, bytes) = self.raw(&format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n",
            self.host
        ));
        assert_eq!(status, 200, "{path}");
        serde_json::from_slice(&bytes).unwrap()
    }

    fn post(&self, path: &str, value: &Value) -> (u16, Value) {
        let body = serde_json::to_string(value).unwrap();
        let (status, bytes) = self.raw(&format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.host, self.host, body.len()
        ));
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn enqueue(&self, path: &str, value: &Value) -> u64 {
        let (status, response) = self.post(path, value);
        assert_eq!(status, 202, "{path} {response}");
        response["id"].as_u64().unwrap()
    }

    fn await_value(&self, path: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let result = self.get(path);
            if ready(&result) {
                return result;
            }
            assert!(Instant::now() < deadline, "timeout for {path}: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn job(&self, path: &str, request: &Value) -> Value {
        let id = self.enqueue(path, request);
        let job = self.await_value("/api/job", |job| job["id"] == id);
        assert!(job["error"].is_null(), "{job}");
        job["value"].clone()
    }

    fn action(&self, request: &Value) {
        let id = self.enqueue("/api/action", request);
        let state = self.await_value("/api/state", |state| state["last_action"]["id"] == id);
        assert_eq!(state["last_action"]["ok"], true, "{state}");
    }

    fn prepare(&self) -> String {
        let setup = self.get("/api/defaults");
        let preview = self.enqueue("/api/preview", &setup);
        let job = self.await_value("/api/job", |job| job["id"] == preview);
        assert!(job["error"].is_null(), "{job}");
        assert_eq!(job["value"]["preview"].as_array().unwrap().len(), 31);
        let request = self.enqueue("/api/run", &json!({"preview_id":preview}));
        let state = self.await_value("/api/state", |state| {
            state["last_action"]["id"] == request && state["run"]["ready"] == true
        });
        assert_eq!(state["run"]["output_state"], "off_verified");
        assert_eq!(state["run"]["resistance_ohms"], json!([10.0, 10.0]));
        state["run"]["id"].as_str().unwrap().to_owned()
    }

    fn run(&self, run: &str) {
        self.action(&json!({"run":run,"kind":"arm"}));
        self.action(&json!({"run":run,"kind":"start","confirmed":true}));
        self.await_value("/api/state", |state| {
            state["run"]["output_state"] == "on_verified"
                && state["run"]["samples"]
                    .as_array()
                    .is_some_and(|samples| samples.iter().any(|row| row["body_k"].is_number()))
        });
    }

    fn stop(&self, run: &str) {
        self.action(&json!({"run":run,"kind":"stop"}));
        self.await_value("/api/state", |state| {
            state["run"]["journal"] == "completed" && state["run"]["output_state"] == "off_verified"
        });
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_response(mut stream: TcpStream) -> (u16, Vec<u8>) {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let header = std::str::from_utf8(&response[..split]).unwrap();
    let code = header
        .split_ascii_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (code, response[split + 4..].to_vec())
}

#[test]
fn historical_head_reports_one_calculation_without_preparing_a_run() {
    let daemon = Daemon::start(false);
    let state = daemon.get("/api/state");
    let job = daemon.get("/api/job");
    assert!(state["run"].is_null() && job.is_null());
    let request = format!(
        "GET /api/historical-head HTTP/1.1\r\nHost: {}\r\n\r\n",
        daemon.host
    );
    let (status, first) = daemon.raw(&request);
    assert_eq!(status, 200);
    assert_eq!(daemon.raw(&request), (status, first.clone()));
    let computed = cvtl_sim::historical_head::report().unwrap();
    assert_eq!(first, serde_json::to_vec(&computed).unwrap());

    let report: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(report["nominal_rows"], json!([1, 2, 3, 4, 5, 6]));
    assert_eq!(report["synthetic"], true);
    assert_eq!(report["test_only"], true);
    let points = report["points"].as_array().unwrap();
    assert_eq!(points.len(), 15);
    for (origin, count) in [
        ("historical_anchor", 6),
        ("interpolated", 5),
        ("synthetic", 4),
    ] {
        assert_eq!(
            points
                .iter()
                .filter(|point| point["origin"] == origin)
                .count(),
            count
        );
    }
    let end = &points[14];
    assert_eq!(end["stage2_k"], 300.0);
    assert_eq!(end["stage1_k"], 300.0);
    assert_eq!(end["origin"], "synthetic");
    assert_eq!(daemon.get("/api/state"), state);
    assert_eq!(daemon.get("/api/job"), job);
    assert_eq!(fs::read_dir(daemon.root.join("runs")).unwrap().count(), 0);
}

#[test]
fn http_requires_local_identity_and_bounded_unambiguous_body() {
    let daemon = Daemon::start(false);
    let request = |headers: &str| {
        format!("POST /api/preview HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nContent-Type: application/json\r\n{headers}\r\n", daemon.host, daemon.host)
    };
    let (status, body) = daemon.raw(&format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", daemon.host));
    assert_eq!(status, 200);
    assert!(body.windows(4).any(|w| w == b"CVTL"));
    assert_eq!(
        daemon.raw("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").0,
        403
    );
    let foreign = format!("POST /api/preview HTTP/1.1\r\nHost: {}\r\nOrigin: http://example.invalid\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}", daemon.host);
    assert_eq!(daemon.raw(&foreign).0, 403);
    assert_eq!(daemon.raw(&request("Content-Length: 65537\r\n")).0, 413);
    assert_eq!(
        daemon
            .raw(&request("Content-Length: 2\r\nContent-Length: 2\r\n"))
            .0,
        400
    );
    assert_eq!(
        daemon.raw(&request("Transfer-Encoding: chunked\r\n")).0,
        400
    );
    assert_eq!(
        daemon
            .raw(&format!(
                "GET /assets/../index.html HTTP/1.1\r\nHost: {}\r\n\r\n",
                daemon.host
            ))
            .0,
        400
    );
    let mut slow = daemon.stream();
    slow.write_all(b"GET / HTTP/1.1\r\nHo").unwrap();
    thread::sleep(Duration::from_millis(350));
    assert_eq!(read_response(slow).0, 408);
}

#[test]
fn live_changes_and_two_completed_runs_are_replayable() {
    let daemon = Daemon::start(false);
    let mut invalid = daemon.get("/api/defaults");
    invalid["body"]["mass_kg"] = json!(0);
    let invalid_id = daemon.enqueue("/api/preview", &invalid);
    let result = daemon.await_value("/api/job", |job| job["id"] == invalid_id);
    assert!(result["error"].is_string());
    assert_eq!(
        daemon.post("/api/run", &json!({"preview_id":invalid_id})).0,
        409
    );
    let first = daemon.prepare();
    assert_eq!(
        daemon
            .post("/api/action", &json!({"run":"stale-run","kind":"arm"}))
            .0,
        409
    );
    let (status, _) = daemon.post(
        "/api/action",
        &json!({"run":first,"kind":"start","confirmed":false}),
    );
    assert!((400..500).contains(&status));
    daemon.run(&first);
    daemon
        .action(&json!({"run":first,"kind":"change","mass_kg":16.0,"scale":0.15,"g_w_per_k":0.04}));
    daemon.await_value("/api/state", |state| {
        state["run"]["samples"].as_array().is_some_and(|samples| {
            samples
                .iter()
                .any(|sample| sample["seq"].is_number() && sample["change"]["mass_kg"] == 16.0)
        })
    });
    daemon.stop(&first);
    let second = daemon.prepare();
    assert_ne!(first, second);
    daemon.run(&second);
    daemon.stop(&second);

    let list = daemon.job("/api/archive", &json!({"kind":"list"}));
    assert_eq!(list.as_array().unwrap().len(), 2);
    let replay = daemon.job("/api/archive", &json!({"kind":"replay","run":first}));
    assert_eq!(replay["receipt_verified"], true);
    assert_eq!(replay["report"]["status"], "matched");
    assert_eq!(replay["report"]["output_state_at_boundary"], "off_verified");
    assert!(replay["samples"]
        .as_array()
        .unwrap()
        .iter()
        .any(|sample| sample["change"]["mass_kg"] == 16.0));
}

#[test]
fn fault_recovery_and_reset_do_not_start_heating() {
    let daemon = Daemon::start(true);
    let run = daemon.prepare();
    daemon.run(&run);
    daemon.action(&json!({"run":run,"kind":"fault"}));
    daemon.await_value("/api/state", |state| {
        state["run"]["state"] == "fault_latched" && state["run"]["output_state"] == "unknown"
    });
    daemon.action(&json!({"run":run,"kind":"recover"}));
    let (status, _) = daemon.post(
        "/api/action",
        &json!({"run":run,"kind":"reset","confirmed":false}),
    );
    assert!((400..500).contains(&status));
    daemon.action(&json!({"run":run,"kind":"reset","confirmed":true}));
    let state = daemon.await_value("/api/state", |state| {
        state["run"]["state"] == "output_off_verified" && state["run"]["fault"].is_null()
    });
    assert_eq!(state["run"]["output_state"], "off_verified");
}

#[test]
fn process_restart_requires_new_arm_and_start() {
    let mut daemon = Daemon::start(false);
    let first = daemon.prepare();
    let first_path = daemon.root.join("runs").join(&first);
    daemon.run(&first);
    assert_eq!(daemon.get("/api/state")["run"]["state"], "running");
    await_raw_heating(&first_path);

    let first_pid = daemon.child.id();
    daemon.child.kill().unwrap();
    assert!(!daemon.child.wait().unwrap().success());
    let old_before = archive_files(&first_path);
    assert!(!old_before.contains_key(Path::new("completed.json")));
    let (old_prefix, old_raw) = raw_witness(old_before[Path::new("raw.ndjson.zst")].as_slice());
    assert!(old_prefix.frames > 0 && old_prefix.tail != Tail::CorruptFrame);
    assert!(old_raw.on_verified.unwrap() < old_raw.first_heating.unwrap().0);

    daemon.child = Daemon::spawn(&daemon.root, false);
    daemon.read_host();
    let state = daemon.get("/api/state");
    assert!(state["run"].is_null());
    assert_eq!(
        daemon
            .post("/api/action", &json!({"run": first, "kind":"arm"}))
            .0,
        409
    );
    assert_eq!(
        daemon.job("/api/archive", &json!({"kind":"list"})),
        json!([])
    );

    let second = daemon.prepare();
    assert_ne!(first, second);
    let request = daemon.enqueue(
        "/api/action",
        &json!({"run": second, "kind":"start", "confirmed":true}),
    );
    let rejected = daemon.await_value("/api/state", |state| state["last_action"]["id"] == request);
    assert_eq!(rejected["last_action"]["ok"], false, "{rejected}");
    assert_eq!(rejected["run"]["output_state"], "off_verified");
    daemon.await_value("/api/state", |state| {
        state["run"]["output_state"] == "off_verified"
            && state["run"]["samples_seen"].as_u64().unwrap() >= 3
    });
    daemon.action(&json!({"run":second,"kind":"arm"}));
    let armed = daemon.get("/api/state");
    assert_eq!(armed["run"]["state"], "armed");
    assert_eq!(armed["run"]["output_state"], "off_verified");
    daemon.action(&json!({"run":second,"kind":"start","confirmed":true}));
    let second_path = daemon.root.join("runs").join(&second);
    await_raw_heating(&second_path);
    daemon.stop(&second);

    let list = daemon.job("/api/archive", &json!({"kind":"list"}));
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["run"], second);
    let replay = daemon.job("/api/archive", &json!({"kind":"replay","run":second}));
    assert_eq!(replay["receipt_verified"], true, "{replay}");
    assert_eq!(replay["report"]["status"], "matched", "{replay}");
    assert_eq!(replay["report"]["output_state_at_boundary"], "off_verified");
    let (new_prefix, new_raw) =
        raw_witness(fs::File::open(second_path.join("raw.ndjson.zst")).unwrap());
    assert_eq!(new_prefix.tail, Tail::FrameBoundary);
    assert_eq!(replay["receipt"]["last_seq"], new_prefix.records);
    assert!(new_raw.zero_ticks_before_arm >= 3, "{new_raw:?}");
    let on = new_raw.on_verified.unwrap();
    let start = new_raw.start.unwrap();
    assert!(new_raw
        .on_intents
        .iter()
        .all(|seq| seq.is_some_and(|seq| start < seq && seq < on)));
    let (tick, inputs) = new_raw.first_tick.unwrap();
    let (drive_tick, drive) = new_raw.first_positive_drive.unwrap();
    let (heating, _) = new_raw.first_heating.unwrap();
    let order = [
        new_raw.off_verified.unwrap(),
        new_raw.rejected_start.unwrap(),
        new_raw.arm.unwrap(),
        start,
        on,
        inputs,
        new_raw.first_control.unwrap(),
        drive,
        heating,
    ];
    assert_eq!(drive_tick, tick);
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "{new_raw:?}"
    );

    let old_after = archive_files(&first_path);
    assert!(
        old_before == old_after,
        "old run files changed after restart"
    );
    if let Some(dir) = std::env::var_os("CVTL_M9_RESTART_EVIDENCE_DIR") {
        let root = PathBuf::from(dir);
        fs::create_dir(&root).unwrap();
        copy_archive(&root.join(&first), &old_after);
        copy_archive(&root.join(&second), &archive_files(&second_path));
        let before_hashes = archive_hashes(&old_before);
        let after_hashes = archive_hashes(&old_after);
        assert_eq!(before_hashes, after_hashes);
        let report = json!({
            "old": {"run": first, "pid": first_pid, "output_after_kill": "unknown",
                "completed_receipt": false, "prefix_frames": old_prefix.frames,
                "prefix_records": old_prefix.records, "raw_tail": format!("{:?}", old_prefix.tail),
                "raw": old_raw, "before_sha256": before_hashes, "after_sha256": after_hashes},
            "new": {"run": second, "pid": daemon.child.id(), "output_after_arm": "off_verified",
                "prefix_frames": new_prefix.frames, "prefix_records": new_prefix.records,
                "raw": new_raw, "receipt": replay["receipt"], "replay": replay["report"]},
        });
        fs::write(
            root.join("restart.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}

fn await_raw_heating(run: &Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let (_, raw) = raw_witness(fs::File::open(run.join("raw.ndjson.zst")).unwrap());
        if raw
            .on_verified
            .zip(raw.first_heating)
            .is_some_and(|(on, (seq, _))| on < seq)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no verified raw frame with heating: {raw:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Debug, Default, Serialize)]
struct RawWitness {
    off_verified: Option<u64>,
    zero_ticks_before_arm: usize,
    rejected_start: Option<u64>,
    arm: Option<u64>,
    start: Option<u64>,
    on_intents: [Option<u64>; 2],
    on_verified: Option<u64>,
    first_tick: Option<(u64, u64)>,
    first_control: Option<u64>,
    first_positive_drive: Option<(u64, u64)>,
    first_heating: Option<(u64, f64)>,
}

fn raw_witness(reader: impl Read) -> (Recovery, RawWitness) {
    let mut raw = RawWitness::default();
    let mut start_request = None;
    let mut checked_first_tick = false;
    let prefix = recover(reader, |row| match row.event {
        Event::Operator { action: "arm", .. } => raw.arm = Some(row.seq),
        Event::Operator {
            action: "start", ..
        } => {
            if raw.arm.is_none() {
                raw.rejected_start = Some(row.seq);
            } else {
                raw.start = Some(row.seq);
                start_request = Some(row.correlation);
            }
        }
        Event::CommandIntent {
            channel,
            command: Intent::On,
            ..
        } => {
            assert_eq!(Some(row.correlation), start_request);
            raw.on_intents[usize::from(channel == Channel::B)] = Some(row.seq);
        }
        Event::OutputEvidence {
            state: "off_verified",
            ..
        } if raw.off_verified.is_none() => raw.off_verified = Some(row.seq),
        Event::OutputEvidence {
            state: "on_verified",
            ..
        } => {
            assert_eq!(Some(row.correlation), start_request);
            raw.on_verified = Some(row.seq);
        }
        Event::Inputs { frame } => {
            if let Correlation::Tick(tick) = row.correlation {
                let sample = TimelineSample::from_inputs(tick, row.seq, &frame);
                if raw.arm.is_none() && sample.measured_w == [Some(0.0); 2] {
                    raw.zero_ticks_before_arm += 1;
                }
                if let Some(watts) = sample.measured_w[0]
                    .zip(sample.measured_w[1])
                    .map(|(a, b)| a + b)
                    .filter(|&watts| watts > 0.0)
                {
                    raw.first_heating.get_or_insert((row.seq, watts));
                }
                if raw.on_verified.is_some() && !checked_first_tick {
                    checked_first_tick = true;
                    if frame.a1_step_v1.is_some() && frame.model_valid && !frame.missed_tick {
                        raw.first_tick = Some((tick, row.seq));
                    }
                }
            }
        }
        Event::Control {
            limited_w: Some(_),
            fault: None,
            ..
        } if raw
            .first_tick
            .is_some_and(|(tick, _)| row.correlation == Correlation::Tick(tick)) =>
        {
            raw.first_control = Some(row.seq);
        }
        Event::CommandIntent {
            command: Intent::Drive { current_a },
            ..
        } if current_a > 0.0 => {
            if let Correlation::Tick(tick) = row.correlation {
                raw.first_positive_drive.get_or_insert((tick, row.seq));
            }
        }
        _ => {}
    })
    .unwrap();
    (prefix, raw)
}

fn archive_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let path = entry.path();
            if kind.is_dir() {
                collect(root, &path, files);
            } else {
                assert!(kind.is_file());
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn copy_archive(dir: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) {
    for (relative, bytes) in files {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

fn archive_hashes(files: &BTreeMap<PathBuf, Vec<u8>>) -> BTreeMap<PathBuf, String> {
    files
        .iter()
        .map(|(path, bytes)| {
            let mut command = if cfg!(target_os = "macos") {
                let mut command = Command::new("shasum");
                command.args(["-a", "256"]);
                command
            } else {
                Command::new("sha256sum")
            };
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            let hex = std::str::from_utf8(&output.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap();
            assert_eq!(hex.len(), 64);
            (path.clone(), hex.to_owned())
        })
        .collect()
}
