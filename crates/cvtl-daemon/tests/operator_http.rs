use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

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
        let mut command = Command::new(env!("CARGO_BIN_EXE_cvtl-daemon"));
        command
            .arg("serve")
            .arg("--assets")
            .arg(&assets)
            .arg("--runs")
            .arg(root.join("runs"))
            .args(["--port", "0"]);
        if test_faults {
            command.arg("--test-faults");
        }
        let child = command.stdout(Stdio::piped()).spawn().unwrap();
        let mut server = Self {
            child,
            root,
            host: String::new(),
        };
        let mut line = String::new();
        BufReader::new(server.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        server.host = line
            .trim()
            .strip_prefix("http://")
            .expect("daemon localhost URL")
            .to_owned();
        assert!(server.host.starts_with("127.0.0.1:"));
        server
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
