// FILE: crates/cvtl-store/src/writer.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Write independent checked Zstandard frames
//   SCOPE: Single worker, setup files, raw and derived event writes
//   DEPENDS: session, record, zstd
//   LINKS: M-CVTL-STORE
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   spawn_file - start actual file worker
//   spawn_with_sink - start writer with an injectable sink
//   Sink - writer-side I/O and measurement boundary
// END_MODULE_MAP
//! Single file worker. Frontiers only move after their corresponding I/O succeeds.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    ops::Range,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use crate::{
    record::{valid_run, Snapshot},
    session::{Batch, Failure, Session, WriterFault, WriterPort},
};

pub const FRAME_BYTES: usize = 256 * 1024;
const FRAME_INTERVAL: Duration = Duration::from_millis(250);
const SYNC_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug)]
pub enum IoSample {
    Queue(Duration),
    Frame {
        encode: Duration,
        write: Duration,
        flush: Duration,
        raw_bytes: usize,
        compressed_bytes: usize,
    },
    Sync(Duration),
}

/// A sequential raw sink and its optional derived event projection.
pub trait Sink: Write {
    fn sync_all(&mut self) -> io::Result<()>;
    fn event_line(&mut self, line: &[u8]) -> io::Result<()>;
    fn events_available(&self) -> bool {
        true
    }
    fn sample(&mut self, _sample: IoSample) {}
}

pub fn spawn_file(
    run: &str,
    generation: u64,
    path: PathBuf,
    snapshot: Snapshot,
    clock: impl Fn() -> Duration + 'static,
) -> Result<Session, Failure> {
    let id = run.to_owned();
    spawn_with_sink(run, generation, clock, move || {
        FileSink::open(&path, &id, generation, snapshot)
    })
}

/// The factory runs on the writer thread. No producer access to the sink.
pub fn spawn_with_sink<S: Sink + Send + 'static>(
    run: &str,
    generation: u64,
    clock: impl Fn() -> Duration + 'static,
    open: impl FnOnce() -> io::Result<S> + Send + 'static,
) -> Result<Session, Failure> {
    let (mut owner, mut port) = Session::channel(run, generation, clock)?;
    let worker = thread::Builder::new()
        .name("cvtl-recording".into())
        .spawn(move || {
            let result = open()
                .map_err(|_| WriterFault::Open)
                .and_then(|sink| run_writer(&mut port, sink));
            if let Err(fault) = result {
                port.fail(fault);
            }
        })
        .map_err(|_| Failure::Writer(WriterFault::Open))?;
    owner.attach_worker(worker);
    Ok(owner)
}

fn run_writer(port: &mut WriterPort, sink: impl Sink) -> Result<(), WriterFault> {
    let mut writer = Writer::new(sink);
    if !writer.events_ok {
        port.events_failed();
    }
    port.ready()?;
    loop {
        writer.service_due(port, Instant::now())?;
        if let Some(batch) = port.next_batch()? {
            writer.add_batch(port, batch)?;
        } else if port.closing() {
            // The close signal is observed after all producer pushes. Read the
            // queue again so a push racing the first empty poll is not lost.
            if let Some(batch) = port.next_batch()? {
                writer.add_batch(port, batch)?;
            } else {
                writer.sync(port)?;
                return port.finish(writer.durable);
            }
        } else if let Some(deadline) = writer.next_deadline() {
            thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        } else {
            thread::park();
        }
    }
}

struct Writer<S> {
    sink: S,
    frame: Vec<u8>,
    event_ranges: Vec<Range<usize>>,
    last_seq: u64,
    written: u64,
    durable: u64,
    frame_due: Option<Instant>,
    sync_due: Option<Instant>,
    events_ok: bool,
}

impl<S: Sink> Writer<S> {
    fn new(sink: S) -> Self {
        let events_ok = sink.events_available();
        Self {
            sink,
            frame: Vec::with_capacity(FRAME_BYTES),
            event_ranges: Vec::new(),
            last_seq: 0,
            written: 0,
            durable: 0,
            frame_due: None,
            sync_due: None,
            events_ok,
        }
    }

    fn add_batch(&mut self, port: &mut WriterPort, batch: Batch) -> Result<(), WriterFault> {
        self.sink.sample(IoSample::Queue(batch.queue_latency()));
        for line in batch.lines() {
            self.service_due(port, Instant::now())?;
            if self.frame.len() + line.bytes().len() > FRAME_BYTES {
                self.flush_frame(port)?;
            }
            let now = Instant::now();
            self.frame_due.get_or_insert(now + FRAME_INTERVAL);
            self.sync_due.get_or_insert(now + SYNC_INTERVAL);
            if self.events_ok && line.in_operator_view() {
                let start = self.frame.len();
                self.event_ranges.push(start..start + line.bytes().len());
            }
            self.frame.extend_from_slice(line.bytes());
            self.last_seq += 1;
        }
        debug_assert_eq!(self.last_seq, batch.last_seq());
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.frame_due.into_iter().chain(self.sync_due).min()
    }

    fn service_due(&mut self, port: &mut WriterPort, now: Instant) -> Result<(), WriterFault> {
        if self.sync_due.is_some_and(|deadline| now >= deadline) {
            self.sync(port)?;
        } else if self.frame_due.is_some_and(|deadline| now >= deadline) {
            self.flush_frame(port)?;
        }
        Ok(())
    }

    fn flush_frame(&mut self, port: &mut WriterPort) -> Result<(), WriterFault> {
        if self.frame.is_empty() {
            return Ok(());
        }
        let start = Instant::now();
        let mut encoder =
            zstd::stream::Encoder::new(Vec::new(), 1).map_err(|_| WriterFault::Write)?;
        encoder
            .include_checksum(true)
            .map_err(|_| WriterFault::Write)?;
        encoder.window_log(18).map_err(|_| WriterFault::Write)?;
        encoder
            .set_pledged_src_size(Some(self.frame.len() as u64))
            .map_err(|_| WriterFault::Write)?;
        encoder
            .write_all(&self.frame)
            .map_err(|_| WriterFault::Write)?;
        let compressed = encoder.finish().map_err(|_| WriterFault::Write)?;
        let encode = start.elapsed();
        let start = Instant::now();
        self.sink
            .write_all(&compressed)
            .map_err(|_| WriterFault::Write)?;
        let write = start.elapsed();
        let start = Instant::now();
        self.sink.flush().map_err(|_| WriterFault::Flush)?;
        let flush = start.elapsed();
        self.sink.sample(IoSample::Frame {
            encode,
            write,
            flush,
            raw_bytes: self.frame.len(),
            compressed_bytes: compressed.len(),
        });
        self.written = self.last_seq;
        port.written(self.written)?;

        for range in self.event_ranges.drain(..) {
            if self.sink.event_line(&self.frame[range]).is_err() {
                self.events_ok = false;
                port.events_failed();
                break;
            }
        }
        self.frame.clear();
        self.frame_due = None;
        Ok(())
    }

    fn sync(&mut self, port: &mut WriterPort) -> Result<(), WriterFault> {
        self.flush_frame(port)?;
        if self.written > self.durable {
            let start = Instant::now();
            self.sink.sync_all().map_err(|_| WriterFault::Sync)?;
            self.sink.sample(IoSample::Sync(start.elapsed()));
            self.durable = self.written;
            port.synced(self.durable)?;
        }
        self.sync_due = None;
        Ok(())
    }
}

struct FileSink {
    raw: File,
    events: Option<File>,
}

impl FileSink {
    fn open(path: &Path, run: &str, generation: u64, snapshot: Snapshot) -> io::Result<Self> {
        let metadata = snapshot
            .metadata(run, generation)
            .map_err(io::Error::other)?;
        if snapshot.references.iter().any(|(name, _)| !valid_run(name)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "недопустимое имя справочника",
            ));
        }
        fs::create_dir(path)?;
        write_snapshot(
            &path.join("config.snapshot.yaml"),
            snapshot.config_yaml.as_bytes(),
        )?;
        if !snapshot.references.is_empty() {
            let root = path.join("references");
            fs::create_dir(&root)?;
            for (name, bytes) in &snapshot.references {
                write_snapshot(&root.join(name), bytes)?;
            }
        }
        write_snapshot(&path.join("meta.json"), &metadata)?;
        let raw = create(&path.join("raw.ndjson.zst"))?;
        let events = create(&path.join("events.ndjson")).ok();
        Ok(Self { raw, events })
    }
}

fn create(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn write_snapshot(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

impl Write for FileSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.raw.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.raw.flush()
    }
}
impl Sink for FileSink {
    fn sync_all(&mut self) -> io::Result<()> {
        self.raw.sync_all()
    }
    fn event_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.events
            .as_mut()
            .ok_or_else(|| io::Error::other("events.ndjson недоступен"))?
            .write_all(line)
    }
    fn events_available(&self) -> bool {
        self.events.is_some()
    }
}
