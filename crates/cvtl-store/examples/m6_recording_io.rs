//! Real filesystem measurements with the RECORDING worker and synthetic input.

use std::{
    borrow::Cow,
    cell::Cell,
    collections::VecDeque,
    error::Error,
    fs::{self, File},
    io::{self, Write},
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_store::{
    record::{encode_line, Correlation, Event, Record, MAX_RECORD_BYTES},
    session::{CloseResult, Session, Status},
    writer::{spawn_with_sink, IoSample, Sink},
};
use serde_json::{json, Value};

const NOMINAL: u64 = 100;
const BURST: u64 = 200;
const SAMPLE_LIMIT: usize = 4096;

struct FileProbe {
    raw: File,
    events: File,
    samples: Arc<Mutex<Vec<IoSample>>>,
}
impl Write for FileProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.raw.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.raw.flush()
    }
}
impl Sink for FileProbe {
    fn sync_all(&mut self) -> io::Result<()> {
        self.raw.sync_all()
    }
    fn event_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.events.write_all(line)
    }
    fn sample(&mut self, sample: IoSample) {
        let mut samples = self.samples.lock().unwrap();
        if samples.len() < SAMPLE_LIMIT {
            samples.push(sample);
        }
    }
}

fn statistics(mut times: Vec<Duration>) -> Value {
    assert!(!times.is_empty());
    times.sort_unstable();
    let percentile = |percentage: usize| {
        let index = (times.len() * percentage).div_ceil(100).saturating_sub(1);
        times[index].as_secs_f64() * 1000.0
    };
    json!({"samples": times.len(), "p50_ms": percentile(50), "p95_ms": percentile(95),
           "p99_ms": percentile(99), "max_ms": percentile(100)})
}

#[derive(Default)]
struct AdmissionStats {
    pending: VecDeque<(u64, Duration)>,
    durable: Vec<Duration>,
    peak_records: u64,
    peak_bytes: u64,
}

impl AdmissionStats {
    fn observe(&mut self, status: Status, now: Duration) -> Result<(), Box<dyn Error>> {
        if let Some(failure) = status.failure {
            return Err(failure.into());
        }
        if status.events_failed {
            return Err("Ошибка проекции событий".into());
        }
        while self
            .pending
            .front()
            .is_some_and(|(seq, _)| *seq <= status.durable)
        {
            let (_, accepted) = self.pending.pop_front().unwrap();
            self.durable.push(now - accepted);
        }
        self.peak_records = self.peak_records.max(status.queued_records);
        self.peak_bytes = self.peak_bytes.max(status.queued_bytes);
        Ok(())
    }
}

fn payload() -> String {
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut state = 7_u64;
    (0..MAX_RECORD_BYTES)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            alphabet[(state >> 32) as usize % alphabet.len()] as char
        })
        .collect()
}

fn burst_record<'a>(payload: &'a str, seq: u64, at_ns: u64) -> Event<'a> {
    let dispatch = |command| Event::Dispatch {
        source: "benchmark",
        command: Cow::Borrowed(command),
        status: "sent",
        at_ns,
    };
    let base = Record::new("io", 1, seq, at_ns, Correlation::Tick(seq), dispatch(""));
    let overhead = encode_line(&base).expect("valid benchmark envelope").len();
    dispatch(&payload[..MAX_RECORD_BYTES - overhead])
}

fn observe(
    owner: &mut Session,
    stats: &mut AdmissionStats,
    clock: &Cell<Duration>,
    epoch: Instant,
) -> Result<Status, Box<dyn Error>> {
    clock.set(epoch.elapsed());
    let status = owner.poll();
    stats.observe(status, clock.get())?;
    Ok(status)
}

fn main() -> Result<(), Box<dyn Error>> {
    let directory = std::env::temp_dir().join(format!(
        "cvtl-m6-io-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&directory)?;
    let sink_dir = directory.clone();
    let samples = Arc::new(Mutex::new(Vec::with_capacity(SAMPLE_LIMIT)));
    let sink_samples = samples.clone();
    let epoch = Instant::now();
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let source = clock.clone();
    let mut owner = spawn_with_sink(
        "io",
        1,
        move || source.get(),
        move || {
            Ok(FileProbe {
                raw: File::create_new(sink_dir.join("raw.ndjson.zst"))?,
                events: File::create_new(sink_dir.join("events.ndjson"))?,
                samples: sink_samples,
            })
        },
    )?;
    let mut stats = AdmissionStats::default();
    while !observe(&mut owner, &mut stats, &clock, epoch)?.ready {
        if epoch.elapsed() >= Duration::from_secs(2) {
            return Err("Нет готовности writer-а".into());
        }
        thread::park_timeout(Duration::from_millis(1));
    }

    let corpus = payload();
    let mut accepts = Vec::with_capacity((NOMINAL + BURST) as usize);
    let start = Instant::now();
    let mut missed = 0;
    for index in 0..NOMINAL + BURST {
        let offset_ms = if index < NOMINAL {
            index * 20
        } else {
            NOMINAL * 20 + (index - NOMINAL) * 10
        };
        let due = start + Duration::from_millis(offset_ms);
        while let Some(remaining) = due.checked_duration_since(Instant::now()) {
            observe(&mut owner, &mut stats, &clock, epoch)?;
            thread::park_timeout(remaining.min(Duration::from_millis(1)));
        }
        let period = Duration::from_millis(if index < NOMINAL { 20 } else { 10 });
        missed += u64::from(Instant::now().saturating_duration_since(due) >= period);
        let seq = index + 1;
        let accepted = epoch.elapsed();
        clock.set(accepted);
        let at_ns = accepted.as_nanos().try_into()?;
        let event = if index < NOMINAL {
            Event::Operator {
                action: "benchmark_sample",
                at_ns,
            }
        } else {
            burst_record(&corpus, seq, at_ns)
        };
        let before = Instant::now();
        owner.accept(Correlation::Tick(seq), &[event])?;
        accepts.push(before.elapsed());
        stats.pending.push_back((seq, accepted));
        observe(&mut owner, &mut stats, &clock, epoch)?;
    }

    clock.set(epoch.elapsed());
    owner.begin_close();
    let completion = loop {
        observe(&mut owner, &mut stats, &clock, epoch)?;
        match owner.poll_close() {
            CloseResult::Completed(proof) => break proof,
            CloseResult::Pending => thread::park_timeout(Duration::from_millis(1)),
            result => return Err(format!("Файловый замер не завершён: {result:?}").into()),
        }
    };
    observe(&mut owner, &mut stats, &clock, epoch)?;
    assert_eq!(completion.last_seq, NOMINAL + BURST);
    assert_eq!(stats.durable.len(), (NOMINAL + BURST) as usize);

    let samples = samples.lock().unwrap();
    assert!(samples.len() < SAMPLE_LIMIT);
    let (mut queue, mut encode, mut write, mut flush, mut sync) =
        (vec![], vec![], vec![], vec![], vec![]);
    let (mut max_frame, mut max_compressed) = (0, 0);
    for sample in samples.iter().copied() {
        match sample {
            IoSample::Queue(elapsed) => queue.push(elapsed),
            IoSample::Frame {
                encode: e,
                write: w,
                flush: f,
                raw_bytes,
                compressed_bytes,
            } => {
                encode.push(e);
                write.push(w);
                flush.push(f);
                max_frame = max_frame.max(raw_bytes);
                max_compressed = max_compressed.max(compressed_bytes);
            }
            IoSample::Sync(elapsed) => sync.push(elapsed),
        }
    }
    assert_eq!(queue.len(), (NOMINAL + BURST) as usize);
    println!(
        "{}",
        json!({
            "directory": directory, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "elapsed_s": start.elapsed().as_secs_f64(),
            "nominal_records": NOMINAL, "nominal_rate_per_s": 50,
            "burst_records": BURST, "burst_rate_per_s": 100, "burst_record_bytes": MAX_RECORD_BYTES,
            "peak_queue_records": stats.peak_records, "peak_queue_bytes": stats.peak_bytes,
            "max_frame_bytes": max_frame, "max_compressed_frame_bytes": max_compressed,
            "missed_schedule": missed, "deadline_violation_fraction": 0.0,
            "accept": statistics(accepts), "queue": statistics(queue), "encode": statistics(encode),
            "write": statistics(write), "flush": statistics(flush), "sync": statistics(sync),
            "accepted_to_published_durable": statistics(stats.durable),
        })
    );
    Ok(())
}
