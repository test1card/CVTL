use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use cvtl_store::{
    record::{Correlation, Event},
    session::{Failure, Session, WriterFault},
    writer::{spawn_with_sink, Sink},
};

#[derive(Clone, Copy)]
enum Inject {
    None,
    Write(i32),
    Flush,
    Sync,
    View,
    Panic,
}

#[derive(Default)]
struct Memory {
    raw: Vec<u8>,
    view: Vec<u8>,
    flushes: usize,
    syncs: usize,
}

struct TestSink {
    state: Arc<Mutex<Memory>>,
    inject: Inject,
}
impl Write for TestSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        assert!(
            !matches!(self.inject, Inject::Panic),
            "injected writer panic"
        );
        if let Inject::Write(errno) = self.inject {
            return Err(io::Error::from_raw_os_error(errno));
        }
        let n = bytes.len().min(7);
        self.state
            .lock()
            .unwrap()
            .raw
            .extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().flushes += 1;
        if matches!(self.inject, Inject::Flush) {
            return Err(io::Error::other("flush"));
        }
        Ok(())
    }
}
impl Sink for TestSink {
    fn sync_all(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().syncs += 1;
        if matches!(self.inject, Inject::Sync) {
            return Err(io::Error::other("sync"));
        }
        Ok(())
    }
    fn event_line(&mut self, line: &[u8]) -> io::Result<()> {
        if matches!(self.inject, Inject::View) {
            return Err(io::Error::other("events"));
        }
        self.state.lock().unwrap().view.extend_from_slice(line);
        Ok(())
    }
}

fn until(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "worker did not publish its result"
        );
        thread::park_timeout(Duration::from_millis(1));
    }
}

fn owner(inject: Inject) -> (Session, Arc<Mutex<Memory>>) {
    let state = Arc::new(Mutex::new(Memory::default()));
    let sink = TestSink {
        state: state.clone(),
        inject,
    };
    let start = Instant::now();
    let mut session = spawn_with_sink("run", 1, move || start.elapsed(), move || Ok(sink)).unwrap();
    until(|| session.poll().ready);
    (session, state)
}

fn admit(session: &mut Session) {
    session
        .accept(
            Correlation::Request(9),
            &[Event::Operator {
                action: "start",
                at_ns: 0,
            }],
        )
        .unwrap();
}

#[test]
fn writer_completes_short_writes_before_publishing_and_projects_events() {
    let (mut session, state) = owner(Inject::None);
    admit(&mut session);
    session.begin_close();
    until(|| session.poll().durable == 1);
    let state = state.lock().unwrap();
    assert_eq!(state.flushes, 1);
    assert_eq!(state.syncs, 1);
    assert_eq!(
        zstd::stream::decode_all(state.raw.as_slice()).unwrap(),
        state.view
    );
    assert_eq!(cvtl_store::record::decode_line(&state.view).unwrap().seq, 1);
}

#[test]
fn writer_preserves_frontiers_on_write_flush_and_sync_failure() {
    for (inject, expected, written) in [
        (Inject::Write(5), WriterFault::Write, 0),
        (Inject::Write(28), WriterFault::Write, 0),
        (Inject::Flush, WriterFault::Flush, 0),
        (Inject::Sync, WriterFault::Sync, 1),
        (Inject::Panic, WriterFault::Panic, 0),
    ] {
        let (mut session, _) = owner(inject);
        admit(&mut session);
        session.begin_close();
        until(|| session.poll().failure.is_some());
        let status = session.poll();
        assert_eq!(status.failure, Some(Failure::Writer(expected)));
        assert_eq!(
            (status.accepted, status.written, status.durable),
            (1, written, 0)
        );
    }
}

#[test]
fn writer_keeps_raw_when_derived_view_fails() {
    let (mut session, state) = owner(Inject::View);
    admit(&mut session);
    session.begin_close();
    until(|| session.poll().durable == 1);
    assert!(session.poll().events_failed);
    assert_eq!(session.poll().failure, None);
    let state = state.lock().unwrap();
    assert!(state.view.is_empty());
    let raw = zstd::stream::decode_all(state.raw.as_slice()).unwrap();
    assert_eq!(cvtl_store::record::decode_line(&raw).unwrap().seq, 1);
}

#[test]
fn close_timeout_is_terminal_even_if_writer_finishes_later() {
    use cvtl_store::session::CloseResult;
    use std::{cell::Cell, rc::Rc, sync::mpsc};

    struct Stall(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Write for Stall {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Sink for Stall {
        fn sync_all(&mut self) -> io::Result<()> {
            self.0.send(()).unwrap();
            self.1.recv().unwrap();
            Ok(())
        }
        fn event_line(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
    }

    let (enter, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let source = clock.clone();
    let mut session = spawn_with_sink(
        "run",
        1,
        move || source.get(),
        move || Ok(Stall(enter, released)),
    )
    .unwrap();
    until(|| session.poll().ready);
    admit(&mut session);
    session.begin_close();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(session.poll_close(), CloseResult::Pending);
    clock.set(Duration::from_secs(2));
    assert_eq!(session.poll_close(), CloseResult::TimedOut);
    release.send(()).unwrap();
    until(|| session.poll().durable == 1);
    assert_eq!(session.poll_close(), CloseResult::TimedOut);
    assert!(!session.poll().ready);
}

#[test]
fn close_requires_successful_final_sync() {
    use cvtl_store::session::CloseResult;
    let (mut session, _) = owner(Inject::None);
    admit(&mut session);
    session.begin_close();
    until(|| session.poll_close() != CloseResult::Pending);
    assert!(matches!(session.poll_close(), CloseResult::Completed(proof) if proof.last_seq == 1));

    let (mut session, _) = owner(Inject::Sync);
    admit(&mut session);
    session.begin_close();
    until(|| session.poll_close() != CloseResult::Pending);
    assert_eq!(
        session.poll_close(),
        CloseResult::Failed(Failure::Writer(WriterFault::Sync))
    );
}

fn frame(seq: u64, checksum: bool) -> Vec<u8> {
    use cvtl_store::record::{encode_line, Record};
    let line = encode_line(&Record::new(
        "run",
        1,
        seq,
        seq,
        Correlation::Run,
        Event::Operator {
            action: "start",
            at_ns: 0,
        },
    ))
    .unwrap();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder
        .set_pledged_src_size(Some(line.len() as u64))
        .unwrap();
    encoder.write_all(&line).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn recovery_checks_frames_and_never_skips_corruption() {
    use cvtl_store::recovery::{recover, Tail};
    let first = frame(1, true);
    let second = frame(2, true);
    let third = frame(3, true);
    let run = [first.clone(), second.clone(), third.clone()].concat();
    let mut rows = Vec::new();
    let good = recover(run.as_slice(), |record| rows.push(record.seq)).unwrap();
    assert_eq!(
        (good.frames, good.records, good.tail),
        (3, 3, Tail::FrameBoundary)
    );
    assert_eq!(rows, [1, 2, 3]);

    let cut = [first.clone(), second[..second.len() - 2].to_vec()].concat();
    let short = recover(cut.as_slice(), |_| ()).unwrap();
    assert_eq!((short.records, short.tail), (1, Tail::IncompleteFrame));

    let mut corrupt = second;
    *corrupt.last_mut().unwrap() ^= 1;
    let damaged = [first.clone(), corrupt, third.clone()].concat();
    let checked = recover(damaged.as_slice(), |_| ()).unwrap();
    assert_eq!((checked.records, checked.tail), (1, Tail::CorruptFrame));

    let unchecked = [first.clone(), frame(2, false)].concat();
    let checked = recover(unchecked.as_slice(), |_| ()).unwrap();
    assert_eq!((checked.records, checked.tail), (1, Tail::CorruptFrame));

    let gap = [first, third].concat();
    let checked = recover(gap.as_slice(), |_| ()).unwrap();
    assert_eq!((checked.records, checked.tail), (1, Tail::CorruptFrame));
}

struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "cvtl-recording-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn actual_session(path: std::path::PathBuf) -> Session {
    use cvtl_store::{record::Snapshot, writer::spawn_file};
    let start = Instant::now();
    spawn_file(
        "run",
        1,
        path,
        Snapshot {
            config_yaml: "hardware_enabled: false\n".into(),
            scenario: "test_only_recording".into(),
            references: vec![("table".into(), b"abc".to_vec())],
        },
        move || start.elapsed(),
    )
    .unwrap()
}

#[test]
fn file_session_prepares_snapshot_before_admission_and_refuses_overwrite() {
    let temp = Temp::new();
    let path = temp.0.join("run");
    let mut session = actual_session(path.clone());
    until(|| session.poll().ready);
    assert_eq!(
        std::fs::read(path.join("config.snapshot.yaml")).unwrap(),
        b"hardware_enabled: false\n"
    );
    assert_eq!(
        std::fs::read(path.join("references/table")).unwrap(),
        b"abc"
    );
    assert!(path.join("meta.json").is_file());
    let mut duplicate = actual_session(path.clone());
    until(|| duplicate.poll().failure.is_some());
    assert_eq!(
        duplicate.poll().failure,
        Some(Failure::Writer(WriterFault::Open))
    );
    assert!(!duplicate.poll().ready);
    assert_eq!(
        std::fs::read(path.join("references/table")).unwrap(),
        b"abc"
    );
}

#[test]
fn recovery_reads_only_verified_raw_after_killed_process() {
    use cvtl_store::recovery::{recover, Tail};
    use std::{
        fs,
        process::{Command, Stdio},
    };
    let temp = Temp::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "recording_subprocess"])
        .env("CVTL_RECORDING_CHILD_DIR", &temp.0)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !temp.0.join("durable").exists() {
        if started.elapsed() >= Duration::from_secs(4) || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("recording child failed to publish durable acknowledgement");
        }
        thread::park_timeout(Duration::from_millis(2));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let raw = temp.0.join("run/raw.ndjson.zst");
    let before = fs::read(&raw).unwrap();
    let mut seqs = Vec::new();
    let result = recover(before.as_slice(), |record| seqs.push(record.seq)).unwrap();
    assert_eq!(seqs.first(), Some(&1));
    assert_eq!(result.tail, Tail::FrameBoundary);
    assert_eq!(before, fs::read(raw).unwrap());
}

#[test]
fn recording_subprocess() {
    let Some(path) = std::env::var_os("CVTL_RECORDING_CHILD_DIR") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let mut session = actual_session(path.join("run"));
    until(|| session.poll().ready);
    admit(&mut session);
    until(|| session.poll().durable == 1);
    std::fs::write(path.join("durable"), b"1").unwrap();
    loop {
        thread::park();
    }
}
