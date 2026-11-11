//! Record the M9 A1 acceptance cases on local emulators.

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_cli::{check_archive, CompletionReceipt, ReceiptOutputState};
use cvtl_core::units::{Kelvin, WattsPerKelvin};
use cvtl_physics::scenario::{BodySetup, BridgeSetup};
use cvtl_safety::{
    machine::{MachineState, OutputState},
    supervisor::OperatorAction,
    FaultReason,
};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::{Fault as LakeFault, LakeShoreSim},
    recording::{RecordingLoop, PERIOD},
};
use cvtl_store::{
    record::{A1StepV1, Channel, Command, Correlation, Demand, Event, PowerSample, Quality},
    recovery::{recover, Tail},
    session::{fault_issue, CloseResult, Completed, Failure, WriterFault},
    writer::{FileSink, Sink},
};
use serde::Serialize;
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const NOMINAL_TICKS: usize = 120;
const STEP_TICK: usize = 50;
const SATURATION_TICKS: usize = 20;
const OFF_QUERIES: [&str; 2] = ["print(smua.source.output)", "print(smub.source.output)"];
const STALE_AGE_MS: u64 = 160;

#[derive(Serialize)]
struct HostTick {
    scheduled_ns: u64,
    started_ns: u64,
    command_latency_ns: Option<u64>,
    measured_w: Option<f64>,
    missed: bool,
}

#[derive(Clone, Copy)]
struct PowerPoint {
    at_ns: u64,
    limited_w: f64,
    measured_w: f64,
}

impl PowerPoint {
    fn error_w(&self) -> f64 {
        (self.measured_w - self.limited_w).abs()
    }
}

struct RawInputs {
    scheduled_ns: u64,
    measured_ns: u64,
    power: [PowerSample; 2],
    requested_w: f64,
    tf_k: f64,
    step: A1StepV1,
}

struct RawControl {
    requested_w: f64,
    power: PowerPoint,
    limited: bool,
    power_limited: bool,
    clean: bool,
}

#[derive(Default)]
struct RawTick {
    inputs: Option<RawInputs>,
    control: Option<RawControl>,
    running: Option<bool>,
}

#[derive(Serialize)]
struct Percentiles {
    median: u64,
    p95: u64,
    p99: u64,
    max: u64,
}

fn main() -> Result<()> {
    let root = output_dir()?;
    println!("{}", json!({"archive_root": root}));
    let nominal = record(
        &root,
        "nominal",
        0.02,
        NOMINAL_TICKS,
        Some((STEP_TICK, 0.04)),
    )?;
    let saturation = record(&root, "saturation", 0.60, SATURATION_TICKS, None)?;
    let summary = json!({"nominal": nominal, "saturation": saturation});
    write_json(&root.join("metrics.json"), &summary)?;
    println!("{summary}");
    let mut faults = serde_json::Map::new();
    for fault in [
        FaultCase::Inversion,
        FaultCase::Stale,
        FaultCase::Source,
        FaultCase::Thermometer,
    ] {
        faults.insert(fault.name().into(), record_fault(&root, fault)?);
    }
    faults.insert("writer".into(), record_writer_failure(&root)?);
    write_json(&root.join("faults.json"), &faults)?;
    println!("{}", Value::Object(faults));
    Ok(())
}

fn output_dir() -> Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let root = match args.next() {
        Some(path) => PathBuf::from(path),
        None => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            env::temp_dir().join(format!("cvtl-m9-a1-{}-{now}", std::process::id()))
        }
    };
    require(
        args.next().is_none(),
        "Укажите один новый каталог свидетельств",
    )?;
    fs::create_dir(&root)?;
    Ok(root.canonicalize()?)
}

fn setup(g: f64) -> BodySetup {
    BodySetup {
        material: "ofhc_rrr100".into(),
        mass_kg: 20.0,
        temperature_k: 30.0,
        scale: 0.1,
        bridge: BridgeSetup::Constant {
            conductance_w_per_k: g,
        },
    }
}

fn start(owner: &mut RecordingLoop<'_>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = owner.poll();
        if let Some(failure) = status.failure {
            return Err(failure.into());
        }
        if status.ready {
            break;
        }
        require(Instant::now() < deadline, "Журнал A1 не готов")?;
        thread::park_timeout(Duration::from_millis(2));
    }
    owner.recover()?;
    owner.operate(OperatorAction::Arm)?;
    owner.operate(OperatorAction::Start)?;
    Ok(())
}

fn wait_for_tick(owner: &mut RecordingLoop<'_>, due: Instant) -> Result<()> {
    while let Some(remaining) = due.checked_duration_since(Instant::now()) {
        if let Some(failure) = owner.poll().failure {
            return Err(failure.into());
        }
        thread::park_timeout(remaining.min(Duration::from_millis(5)));
    }
    Ok(())
}

fn finish(owner: &mut RecordingLoop<'_>, path: &Path, run: &str) -> Result<Completed> {
    owner.begin_close()?;
    require(
        owner.output_state() == OutputState::OffVerified,
        "OFF обеих ветвей не подтверждён",
    )?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let proof = loop {
        match owner.poll_close() {
            CloseResult::Completed(proof) => break proof,
            CloseResult::Pending => {
                require(Instant::now() < deadline, "Срок закрытия журнала истёк")?;
                thread::park_timeout(Duration::from_millis(2));
            }
            result => return Err(format!("Журнал не завершён ({result:?})").into()),
        }
    };
    let receipt = CompletionReceipt {
        run: run.into(),
        generation: proof.generation,
        last_seq: proof.last_seq,
        output_state: ReceiptOutputState::OffVerified,
    };
    write_json(
        &path.join("completed.json"),
        &json!({"receipt": receipt, "diagnostics": {"wiring": [], "fixture": []}}),
    )?;
    Ok(proof)
}

fn check_closed(path: &Path, run: &str, proof: Completed) -> Result<Value> {
    let replay = check_archive(path)?;
    let mut completed: Value = serde_json::from_reader(File::open(path.join("completed.json"))?)?;
    let receipt: CompletionReceipt = serde_json::from_value(completed["receipt"].take())?;
    let report = serde_json::to_value(&replay.report)?;
    require(
        receipt.run == run
            && receipt.generation == proof.generation
            && receipt.last_seq == proof.last_seq
            && receipt.output_state == ReceiptOutputState::OffVerified
            && receipt.matches_raw(&replay)
            && replay.last_verified_seq == proof.last_seq
            && report["status"] == "matched"
            && report["output_state_at_boundary"] == "off_verified",
        format!("Реплей закрытого прогона {run} не подтверждён ({report})"),
    )?;
    Ok(report)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultCase {
    Inversion,
    Stale,
    Source,
    Thermometer,
}

impl FaultCase {
    fn name(self) -> &'static str {
        match self {
            Self::Inversion => "inversion",
            Self::Stale => "stale",
            Self::Source => "source_loss",
            Self::Thermometer => "thermometer_loss",
        }
    }

    fn reason(self) -> FaultReason {
        match self {
            Self::Inversion => FaultReason::HeadInversion,
            Self::Stale => FaultReason::TemperatureStale,
            Self::Source => FaultReason::SourceLinkLost,
            Self::Thermometer => FaultReason::ThermometerLinkLost,
        }
    }

    fn inject(self, source: &KeithleySimulator, lake: &LakeShoreSim) -> io::Result<()> {
        match self {
            Self::Inversion => lake.override_next_kelvin(2, Kelvin::new(47.0).unwrap()),
            Self::Stale => lake.delay_next_status(2, Duration::from_millis(200)),
            Self::Source => {
                source.disconnect_next_query();
                Ok(())
            }
            Self::Thermometer => {
                lake.set_fault(LakeFault::Disconnect);
                Ok(())
            }
        }
    }

    fn output(self) -> OutputState {
        if self == Self::Source {
            OutputState::Unknown
        } else {
            OutputState::OffVerified
        }
    }
}

fn delivered(source: &KeithleySimulator) -> Result<[f64; 2]> {
    Ok(source.delivered_power()?.map(|power| power.get()))
}

fn require_off(owner: &RecordingLoop<'_>, source: &KeithleySimulator) -> Result<()> {
    require(
        owner.output_state() == OutputState::OffVerified && delivered(source)? == [0.0; 2],
        "Требуется подтверждённый OFF и нулевая мощность обеих ветвей",
    )
}

fn drive_ticks(owner: &mut RecordingLoop<'_>, due: &mut Instant, count: usize) -> Result<()> {
    for _ in 0..count {
        wait_for_tick(owner, *due)?;
        let report = owner.tick(*due)?;
        require(
            !report.missed && report.command_latency.is_some(),
            "Штатный такт до или после отказа не подтвердил Drive",
        )?;
        *due += PERIOD;
    }
    Ok(())
}

fn record_fault(root: &Path, fault: FaultCase) -> Result<Value> {
    let run = fault.name();
    let path = root.join(run);
    let source = KeithleySimulator::start()?;
    let lake = LakeShoreSim::spawn()?;
    let epoch = Instant::now();
    let mut owner = if fault == FaultCase::Stale {
        let writer_path = path.clone();
        RecordingLoop::new_a1_acceptance(
            &source,
            &lake,
            run,
            epoch,
            STALE_AGE_MS,
            move |snapshot| FileSink::open(&writer_path, run, 1, snapshot),
        )?
    } else {
        RecordingLoop::new_a1(&source, &lake, run, path.clone(), epoch)?
    };
    start(&mut owner)?;
    let mut due = Instant::now() + PERIOD;
    drive_ticks(&mut owner, &mut due, 4)?;
    let power_before = delivered(&source)?;
    require(
        power_before.iter().sum::<f64>() > 0.0,
        format!("{run}: нет нагрева до отказа"),
    )?;

    wait_for_tick(&mut owner, due)?;
    fault.inject(&source, &lake)?;
    let rejected = owner.tick(due);
    require(
        rejected.is_err()
            && owner.fault_reason() == Some(fault.reason())
            && owner.state() == MachineState::FaultLatched
            && owner.output_state() == fault.output()
            && owner.fallback_requested() == (fault == FaultCase::Source),
        format!(
            "{run}: отказ {rejected:?}, состояние {:?}, причина {:?}, выход {:?}",
            owner.state(),
            owner.fault_reason(),
            owner.output_state()
        ),
    )?;
    let power_at_fault = if fault == FaultCase::Source {
        None
    } else {
        require_off(&owner, &source)?;
        Some(delivered(&source)?)
    };

    let mut recovery = Value::Null;
    if matches!(fault, FaultCase::Source | FaultCase::Thermometer) {
        lake.set_fault(LakeFault::None);
        owner.recover()?;
        require_off(&owner, &source)?;
        owner.operate(OperatorAction::Reset)?;
        require(
            owner.operate(OperatorAction::Start).is_err(),
            "Start прошёл до нового Arm",
        )?;
        require_off(&owner, &source)?;
        owner.operate(OperatorAction::Arm)?;
        require_off(&owner, &source)?;
        owner.operate(OperatorAction::Start)?;
        let mut next = Instant::now() + PERIOD;
        drive_ticks(&mut owner, &mut next, 1)?;
        wait_for_tick(&mut owner, next)?;
        let measured = owner.tick(next)?.measured_w;
        require(
            measured.is_some_and(|w| w > 0.0),
            "После явных Arm и Start нет измеренного нагрева",
        )?;
        recovery = json!({"start_before_arm_rejected": true, "power_after_restart_w": measured});
    }

    let proof = finish(&mut owner, &path, run)?;
    let replay = check_closed(&path, run, proof)?;
    Ok(json!({
        "power_before_w": power_before,
        "power_at_fault_w": power_at_fault,
        "raw_fault": check_fault_tick(&path, fault, nanos(due.duration_since(epoch)))?,
        "manual_recovery": recovery,
        "replay": replay,
    }))
}

fn check_fault_tick(path: &Path, fault: FaultCase, scheduled_ns: u64) -> Result<Value> {
    let mut tick = None;
    let mut facts = Value::Null;
    let mut reason_seq = None;
    let mut off_seq = [0; 2];
    let mut readback_seq = [0; 2];
    let mut evidence = None;
    let mut drive = false;
    let expected = fault_issue(fault.reason()).code;
    let expected_state = if fault == FaultCase::Source {
        "unknown"
    } else {
        "off_verified"
    };
    recover(File::open(path.join("raw.ndjson.zst"))?, |row| {
        if let (Correlation::Tick(id), Event::Inputs { frame }) = (row.correlation, &row.event) {
            if frame.scheduled_ns == scheduled_ns {
                tick = Some(id);
                let age = frame.temperatures[0]
                    .read_started_ns
                    .zip(frame.temperatures[1].read_started_ns)
                    .and_then(|(a, b)| frame.measured_ns.checked_sub(a.min(b)));
                facts = json!({
                    "tick": id,
                    "stage1_k": frame.temperatures[0].temperature_k,
                    "stage2_k": frame.temperatures[1].temperature_k,
                    "head_age_ns": age,
                    "source_connected": frame.source_connected,
                    "thermometer_connected": frame.thermometer_connected,
                    "missed_tick": frame.missed_tick,
                    "model_valid": frame.model_valid,
                });
            }
        }
        if !matches!((tick, row.correlation), (Some(id), Correlation::Tick(found)) if id == found) {
            return;
        }
        match row.event {
            Event::Observation { faults, .. }
                if faults.first().is_some_and(|f| f.code == expected)
                    && faults
                        .iter()
                        .all(|f| !matches!(f.code, "missed_tick" | "model_failure")) =>
            {
                reason_seq = Some(row.seq);
            }
            Event::CommandIntent {
                channel,
                command: Command::Off,
                ..
            } => {
                off_seq[usize::from(channel == Channel::B)] = row.seq;
            }
            Event::CommandIntent {
                command: Command::Drive { .. },
                ..
            } => drive = true,
            Event::Response {
                source: "keithley",
                command,
                status: "received",
                bytes: Some(bytes),
                ..
            } => {
                let off = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    == Some(0.0);
                if off {
                    if let Some(index) = OFF_QUERIES.iter().position(|query| *query == command) {
                        readback_seq[index] = row.seq;
                    }
                }
            }
            Event::OutputEvidence {
                state,
                fault: Some(reason),
                fallback_requested,
                ..
            } if reason == expected => {
                if state == expected_state && fallback_requested == (fault == FaultCase::Source) {
                    evidence = Some(row.seq);
                }
            }
            _ => {}
        }
    })?;
    let reason_seq = reason_seq.ok_or_else(|| format!("{expected}: нет исходной причины в raw"))?;
    let evidence =
        evidence.ok_or_else(|| format!("{expected}: нет доказательства выхода в raw"))?;
    let ordered_off = off_seq
        .iter()
        .all(|&seq| reason_seq < seq && seq < evidence);
    let paired = (0..2).all(|i| off_seq[i] < readback_seq[i] && readback_seq[i] < evidence);
    require(ordered_off && !drive && paired == (fault != FaultCase::Source),
        format!("{expected}: нарушен порядок OFF, intents {off_seq:?}, readback {readback_seq:?}, evidence {evidence}, Drive {drive}"))?;
    if fault == FaultCase::Stale {
        require(
            facts["head_age_ns"]
                .as_u64()
                .is_some_and(|age| age > STALE_AGE_MS * 1_000_000)
                && facts["source_connected"] == true
                && facts["thermometer_connected"] == true
                && facts["missed_tick"] == false
                && facts["model_valid"] == true,
            format!("Возраст или состояние исправного тракта не подтверждены ({facts})"),
        )?;
    }
    if fault == FaultCase::Inversion {
        require(
            facts["stage2_k"] == 47.0,
            "Подмена 47 К не сохранилась в исходных измерениях",
        )?;
    }
    Ok(
        json!({"inputs": facts, "reason": expected, "reason_seq": reason_seq,
        "output_state": expected_state, "fallback_requested": fault == FaultCase::Source,
        "off_intent_seq": off_seq, "off_readback_seq": readback_seq,
        "output_evidence_seq": evidence, "paired_off_readback": paired}),
    )
}

struct FailingFileSink {
    inner: FileSink,
    fail: Arc<AtomicBool>,
}

impl Write for FailingFileSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail.load(Ordering::Acquire) {
            return Err(io::Error::other("M9 test_only: отказ записи raw"));
        }
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Sink for FailingFileSink {
    fn sync_all(&mut self) -> io::Result<()> {
        self.inner.sync_all()
    }

    fn event_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.inner.event_line(line)
    }

    fn events_available(&self) -> bool {
        self.inner.events_available()
    }
}

fn record_writer_failure(root: &Path) -> Result<Value> {
    let path = root.join("writer");
    let source = KeithleySimulator::start()?;
    let lake = LakeShoreSim::spawn()?;
    let epoch = Instant::now();
    let fail = Arc::new(AtomicBool::new(false));
    let writer_path = path.clone();
    let writer_fail = Arc::clone(&fail);
    let mut owner =
        RecordingLoop::new_a1_acceptance(&source, &lake, "writer", epoch, 1500, move |snapshot| {
            let inner = FileSink::open(&writer_path, "writer", 1, snapshot)?;
            Ok(FailingFileSink {
                inner,
                fail: writer_fail,
            })
        })?;
    start(&mut owner)?;
    let mut due = Instant::now() + PERIOD;
    drive_ticks(&mut owner, &mut due, 4)?;
    let power_before = delivered(&source)?;
    require(
        power_before.iter().sum::<f64>() > 0.0,
        "Отказ writer требует исходного нагрева",
    )?;
    let goal = owner.poll().accepted;
    let mut durable = 0;
    for _ in 0..30 {
        drive_ticks(&mut owner, &mut due, 1)?;
        durable = owner.poll().durable;
        if durable >= goal {
            break;
        }
    }
    require(
        durable >= goal && goal > 0,
        "Нет устойчивого префикса перед отказом writer",
    )?;
    let readbacks_before = OFF_QUERIES.map(|query| source.query_count(query));
    wait_for_tick(&mut owner, due)?;
    fail.store(true, Ordering::Release);
    let _ = owner.tick(due);
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        let status = owner.poll();
        if status.failure.is_some() {
            break status;
        }
        require(
            Instant::now() < deadline,
            "Отказ writer не опубликован в пределах срока",
        )?;
        thread::park_timeout(Duration::from_millis(5));
    };
    let failure = Failure::Writer(WriterFault::Write);
    require(
        status.failure == Some(failure)
            && owner.fault_reason() == Some(FaultReason::JournalFailure)
            && owner.state() == MachineState::FaultLatched,
        format!(
            "Некорректный исход отказа writer ({status:?}, {:?})",
            owner.fault_reason()
        ),
    )?;
    require_off(&owner, &source)?;
    let readbacks_after = OFF_QUERIES.map(|query| source.query_count(query));
    require(
        (0..2).all(|i| readbacks_after[i] > readbacks_before[i]),
        "Эмулятор не подтвердил оба OFF после отказа writer",
    )?;
    require(
        owner.tick(Instant::now()).is_err() && owner.poll().accepted == status.accepted,
        "После отказа writer принята новая запись или команда",
    )?;
    require_off(&owner, &source)?;
    let _ = owner.begin_close();
    require(
        owner.poll_close() == CloseResult::Failed(failure),
        "Закрытие отказавшего журнала должно быть Failed",
    )?;

    let mut last_seq = 0;
    let raw = recover(File::open(path.join("raw.ndjson.zst"))?, |row| {
        last_seq = row.seq
    })?;
    require(
        raw.tail == Tail::FrameBoundary
            && raw.records == last_seq
            && durable <= raw.records
            && raw.records < status.accepted
            && !path.join("completed.json").exists(),
        format!(
            "Неверный префикс writer ({raw:?}, durable={durable}, accepted={})",
            status.accepted
        ),
    )?;
    Ok(
        json!({"failure": "writer_write", "reason": "journal_failure", "state": "fault_latched",
        "close": "failed", "power_before_w": power_before,
        "previous_durable_seq": durable, "accepted_at_fault": status.accepted,
        "recovered_frames": raw.frames, "recovered_records": raw.records, "tail": "frame_boundary",
        "live_off_witness": {"output": "off_verified", "power_w": delivered(&source)?,
            "readback_count_before": readbacks_before, "readback_count_after": readbacks_after}}),
    )
}

fn record(
    root: &Path,
    run: &str,
    initial_g: f64,
    ticks: usize,
    change: Option<(usize, f64)>,
) -> Result<Value> {
    let path = root.join(run);
    let source = KeithleySimulator::start()?;
    let thermometer = LakeShoreSim::spawn()?;
    let epoch = Instant::now();
    let mut owner = RecordingLoop::new_a1_configured(
        &source,
        &thermometer,
        run,
        path.clone(),
        epoch,
        &setup(initial_g),
    )?;
    start(&mut owner)?;
    let first_due = Instant::now() + PERIOD;
    let mut host = Vec::with_capacity(ticks);
    for index in 1..=ticks {
        let due = first_due + PERIOD * (index as u32 - 1);
        wait_for_tick(&mut owner, due)?;
        if let Some((at, g)) = change {
            if index == at {
                owner.change_a1(None, WattsPerKelvin::new(g))?;
            }
        }
        let report = owner.tick(due)?;
        host.push(HostTick {
            scheduled_ns: nanos(due.duration_since(epoch)),
            started_ns: nanos(report.started.duration_since(epoch)),
            command_latency_ns: report.command_latency.map(nanos),
            measured_w: report.measured_w,
            missed: report.missed,
        });
    }
    let proof = finish(&mut owner, &path, run)?;
    let host_duration_ns = nanos(epoch.elapsed());
    write_json(&root.join(format!("{run}-host.json")), &host)?;
    let replay = check_closed(&path, run, proof)?;
    let raw = read_ticks(&path)?;
    let points = join_ticks(&raw, &host)?;
    let model_duration_ns = points
        .iter()
        .map(|(input, _)| input.step.interval_ns)
        .sum::<u64>();
    let mut summary = match change {
        Some((at, g)) => nominal_metrics(&points, &host, at, g)?,
        None => saturation_metrics(&points, initial_g)?,
    };
    summary["run"] = json!(run);
    summary["ticks"] = json!(ticks);
    summary["host_duration_ns"] = json!(host_duration_ns);
    summary["model_duration_ns"] = json!(model_duration_ns);
    summary["last_seq"] = json!(proof.last_seq);
    summary["output_state"] = replay["output_state_at_boundary"].clone();
    Ok(summary)
}

fn read_ticks(path: &Path) -> Result<BTreeMap<u64, RawTick>> {
    let mut ticks = BTreeMap::<u64, RawTick>::new();
    let mut unique = true;
    let recovered = recover(File::open(path.join("raw.ndjson.zst"))?, |record| {
        let Correlation::Tick(id) = record.correlation else {
            return;
        };
        let tick = ticks.entry(id).or_default();
        match record.event {
            Event::Inputs { frame } => {
                if let (Some(step), Demand::VirtualBody { requested_w }, Some(tf_k)) = (
                    frame.a1_step_v1,
                    frame.demand,
                    frame.temperatures[2].temperature_k,
                ) {
                    unique &= tick
                        .inputs
                        .replace(RawInputs {
                            scheduled_ns: frame.scheduled_ns,
                            measured_ns: frame.measured_ns,
                            power: frame.power,
                            requested_w,
                            tf_k,
                            step,
                        })
                        .is_none();
                }
            }
            Event::Observation {
                state,
                faults,
                warnings,
                ..
            } => {
                unique &= tick
                    .running
                    .replace(state == "running" && faults.is_empty() && warnings.is_empty())
                    .is_none();
            }
            Event::Control {
                at_ns,
                requested_w: Some(requested_w),
                limited_w: Some(limited_w),
                measured_w: Some(measured_w),
                slew_limited,
                power_limited,
                current_limited,
                warning,
                fault,
                ..
            } => {
                unique &= tick
                    .control
                    .replace(RawControl {
                        requested_w,
                        power: PowerPoint {
                            at_ns,
                            limited_w,
                            measured_w,
                        },
                        limited: slew_limited || power_limited || current_limited,
                        power_limited,
                        clean: warning.is_none() && fault.is_none(),
                    })
                    .is_none();
            }
            _ => {}
        }
    })?;
    require(
        unique && recovered.tail == Tail::FrameBoundary,
        "Повтор или неполный хвост в штатном raw",
    )?;
    Ok(ticks)
}

fn join_ticks<'a>(
    raw: &'a BTreeMap<u64, RawTick>,
    host: &[HostTick],
) -> Result<Vec<(&'a RawInputs, &'a RawControl)>> {
    require(
        raw.len() == host.len(),
        "Число тактов raw и часов хоста не совпадает",
    )?;
    let mut points = Vec::with_capacity(host.len());
    for ((id, tick), host) in raw.iter().zip(host) {
        let input = tick.inputs.as_ref().ok_or("Нет пригодного Inputs")?;
        let control = tick.control.as_ref().ok_or("Нет пригодного Control")?;
        require(
            tick.running == Some(true) && control.clean,
            format!("Нештатное состояние на такте {id}"),
        )?;
        require(
            host.scheduled_ns == input.scheduled_ns && control.power.at_ns == input.measured_ns,
            format!("Несовпадение отметок такта {id}"),
        )?;
        require(
            (input.requested_w - control.requested_w).abs() <= 1e-9,
            format!("Запрос Inputs и Control различается на такте {id}"),
        )?;
        let mut measured_w = 0.0;
        for sample in &input.power {
            require(
                sample.quality == Quality::Measured,
                format!("Непригодное измерение на такте {id}"),
            )?;
            measured_w +=
                sample.current_a.ok_or("Нет тока")? * sample.voltage_v.ok_or("Нет напряжения")?;
        }
        require(
            (measured_w - control.power.measured_w).abs() <= 1e-9
                && host
                    .measured_w
                    .is_some_and(|value| (value - measured_w).abs() <= 1e-9),
            format!("U×I не совпадает с измеренной мощностью на такте {id}"),
        )?;
        points.push((input, control));
    }
    Ok(points)
}

fn nominal_metrics(
    points: &[(&RawInputs, &RawControl)],
    host: &[HostTick],
    step_tick: usize,
    g: f64,
) -> Result<Value> {
    let step_index = step_tick - 1;
    let (step_input, step_control) = points[step_index];
    require(
        step_input
            .step
            .change
            .and_then(|change| change.bridge_w_per_k)
            == Some(g),
        "Принятая ступень G отсутствует",
    )?;
    let step_w = step_control.requested_w - points[step_index - 1].1.requested_w;
    require(
        (0.20..=0.35).contains(&step_w),
        format!("Недостижимая ступень запроса ({step_w} Вт)"),
    )?;
    require(
        points.iter().all(|(_, control)| {
            !control.limited
                && (0.2..=0.8).contains(&control.requested_w)
                && (control.requested_w - control.power.limited_w).abs() <= 1e-9
        }),
        "Штатный запрос ограничен или вышел за диапазон",
    )?;
    let powers = points
        .iter()
        .map(|(_, control)| control.power)
        .collect::<Vec<_>>();
    let settling = settling_ns(&powers, step_index)?;
    require(
        settling <= 2_000_000_000,
        format!("Установление заняло {settling} нс"),
    )?;
    let pre_error = steady_error(&powers[29..49])?;
    let post_error = steady_error(&powers[100..120])?;
    let missed_ticks = host.iter().filter(|tick| tick.missed).count();
    require(
        missed_ticks == 0,
        format!("Пропущено тактов {missed_ticks}"),
    )?;
    let mut jitter = host
        .windows(2)
        .map(|pair| {
            pair[1]
                .started_ns
                .checked_sub(pair[0].started_ns)
                .map(|dt| dt.abs_diff(nanos(PERIOD)))
                .ok_or("Отметки старта тика не монотонны")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut latency = host
        .iter()
        .map(|tick| tick.command_latency_ns.ok_or("Нет подтверждения команды"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let jitter = percentiles(&mut jitter)?;
    let latency = percentiles(&mut latency)?;
    require(
        jitter.p99 <= 20_000_000,
        format!("p99 джиттера {} нс", jitter.p99),
    )?;
    require(
        latency.p99 <= 300_000_000,
        format!("p99 задержки команды {} нс", latency.p99),
    )?;
    Ok(json!({
        "step_tick": step_tick,
        "step_ns": step_control.power.at_ns,
        "step_w": step_w,
        "settling_ns": settling,
        "pre_max_error_w": pre_error,
        "post_max_error_w": post_error,
        "jitter_ns": jitter,
        "command_latency_ns": latency,
        "missed_ticks": missed_ticks,
    }))
}

fn saturation_metrics(points: &[(&RawInputs, &RawControl)], g: f64) -> Result<Value> {
    let sustained = &points[10..20];
    require(
        sustained.iter().all(|(input, control)| {
            control.power_limited
                && input.step.interval_ns > 0
                && control.requested_w > control.power.limited_w
                && control.power.measured_w > 0.0
                && (input.requested_w - g * (input.step.body_temperature_k - input.tf_k)).abs()
                    <= 1e-9
        }),
        "Нет непрерывного насыщения с пригодным измерением на тактах 11–20",
    )?;
    require(
        sustained
            .windows(2)
            .all(|pair| pair[1].0.step.body_temperature_k < pair[0].0.step.body_temperature_k),
        "При насыщении не подтверждено продолжение модели тела",
    )?;
    let (last, control) = sustained[9];
    Ok(json!({
        "power_limited_ticks": sustained.len(),
        "last_requested_w": last.requested_w,
        "last_limited_w": control.power.limited_w,
        "last_measured_w": control.power.measured_w,
    }))
}

fn steady_error(points: &[PowerPoint]) -> Result<f64> {
    let mut maximum = 0.0_f64;
    for point in points {
        let error = point.error_w();
        require(
            error <= (point.limited_w * 0.01).max(0.005),
            format!("Ошибка слежения {error} Вт превышает допуск"),
        )?;
        maximum = maximum.max(error);
    }
    Ok(maximum)
}

fn settling_ns(points: &[PowerPoint], step_index: usize) -> Result<u64> {
    let mut first = points.len();
    for (index, point) in points.iter().enumerate().skip(step_index + 1).rev() {
        if point.error_w() > point.limited_w * 0.02 {
            break;
        }
        first = index;
    }
    require(
        points.len() - first >= 10,
        "Нет десяти последовательных измерений в полосе 2 %",
    )?;
    points[first]
        .at_ns
        .checked_sub(points[step_index].at_ns)
        .ok_or_else(|| "Отметки ступени и установления не монотонны".into())
}

fn percentiles(samples: &mut [u64]) -> Result<Percentiles> {
    require(!samples.is_empty(), "Нет измерений для квантилей")?;
    samples.sort_unstable();
    let rank = |p: usize| samples[(p * samples.len()).div_ceil(100) - 1];
    Ok(Percentiles {
        median: rank(50),
        p95: rank(95),
        p99: rank(99),
        max: rank(100),
    })
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("нс ограниченного испытания A1")
}

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let pending = path.with_extension("pending");
    let mut file = File::create_new(&pending)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(pending, path)?;
    File::open(path.parent().ok_or("Нет каталога результата")?)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_upper_rank() {
        let mut samples: Vec<_> = (1..=101).rev().collect();
        let ranks = percentiles(&mut samples).unwrap();
        assert_eq!(
            [ranks.median, ranks.p95, ranks.p99, ranks.max],
            [51, 96, 100, 101]
        );
        assert!(percentiles(&mut []).is_err());
    }

    #[test]
    fn settling_requires_a_continuous_post_step_suffix() {
        let mut points: Vec<_> = (0..15)
            .map(|index| PowerPoint {
                at_ns: index * 100_000_000,
                limited_w: 0.6,
                measured_w: 0.6,
            })
            .collect();
        points[4].measured_w = 0.58;
        assert_eq!(settling_ns(&points, 1).unwrap(), 400_000_000);
        points[5].measured_w = 0.58;
        assert!(settling_ns(&points, 1).is_err());
    }
}
