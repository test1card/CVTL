use std::{
    borrow::Cow,
    fs,
    time::{Duration, Instant},
};

use cvtl_instruments::{keithley::parse_number, LoopbackTcp, MessageTransport};
use cvtl_safety::{
    machine::{MachineState, OutputState},
    supervisor::OperatorAction,
    FaultReason,
};
use cvtl_sim::{keithley::KeithleySimulator, lakeshore::LakeShoreSim, recording::RecordingLoop};
use cvtl_store::{
    record::{Command, Correlation, Event},
    recovery::{recover, Tail},
    session::{CloseResult, Failure, Session, WriterFault, WriterPort},
    writer::spawn_file,
};

fn reported(sim: &KeithleySimulator, expression: &str) -> f64 {
    let mut wire = LoopbackTcp::connect(sim.addr(), Duration::from_millis(300)).unwrap();
    parse_number(
        &wire
            .query(expression, Instant::now() + Duration::from_millis(300))
            .unwrap(),
    )
    .unwrap()
}

fn off(source: &KeithleySimulator) {
    assert_eq!(reported(source, "print(smua.source.output)"), 0.0);
    assert_eq!(reported(source, "print(smub.source.output)"), 0.0);
}

fn session(epoch: Instant) -> (Session, WriterPort) {
    let (mut owner, mut writer) = Session::channel("m6", 1, move || epoch.elapsed()).unwrap();
    writer.ready().unwrap();
    assert!(owner.poll().ready);
    (owner, writer)
}

fn start(owner: &mut RecordingLoop<'_>) {
    owner.recover().unwrap();
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
    assert_eq!(owner.state(), MachineState::Running);
}

fn wait(mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::park_timeout(Duration::from_millis(2));
    }
}

#[test]
fn normal_loopback_run_records_inputs_intent_dispatch_responses_and_off() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let path = std::env::temp_dir().join(format!("cvtl-m6-loopback-{}", std::process::id()));
    let session = spawn_file(
        "m6",
        1,
        path.clone(),
        RecordingLoop::snapshot(),
        move || epoch.elapsed(),
    )
    .unwrap();
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    wait(|| owner.poll().ready);
    start(&mut owner);
    owner.tick(Instant::now()).unwrap();
    owner.begin_close().unwrap();
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    wait(|| owner.poll_close() != CloseResult::Pending);
    let CloseResult::Completed(completion) = owner.poll_close() else {
        panic!("run not completed");
    };
    drop(owner);
    assert!(reported(&source, "print(smua.source.leveli)") > 0.0);
    off(&source);
    let file = fs::File::open(path.join("raw.ndjson.zst")).unwrap();
    let mut have_input = false;
    let mut have_intent = false;
    let mut have_dispatch = false;
    let mut have_response = false;
    let mut have_control = false;
    let recovered = recover(file, |row| {
        if let Correlation::Tick(_) = row.correlation {
            match row.event {
                Event::Inputs { .. } => have_input = true,
                Event::Control {
                    requested_w: Some(_),
                    limited_w: Some(_),
                    measured_w: Some(_),
                    ..
                } => have_control = true,
                Event::CommandIntent {
                    command: Command::Drive { .. },
                    ..
                } => {
                    assert!(have_input && have_control);
                    have_intent = true;
                }
                Event::Dispatch {
                    command,
                    status: "sent",
                    ..
                } if command.contains(".source.leveli =") => {
                    assert!(have_intent);
                    have_dispatch = true;
                }
                Event::Response {
                    command,
                    bytes: Some(_),
                    ..
                } if command.contains(".source.leveli") => {
                    assert!(have_dispatch);
                    have_response = true;
                }
                _ => {}
            }
        }
    })
    .unwrap();
    assert!(have_response);
    assert_eq!(recovered.tail, Tail::FrameBoundary);
    assert_eq!(recovered.records, completion.last_seq);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn writer_error_after_admission_and_stalled_close_leave_off_available() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (session, writer) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    start(&mut owner);
    owner.tick(Instant::now()).unwrap();
    writer.fail(WriterFault::Sync);
    let status = owner.poll();
    assert!(status.accepted > 0);
    assert_eq!(status.failure, Some(Failure::Writer(WriterFault::Sync)));
    assert_eq!(owner.fault_reason(), Some(FaultReason::JournalFailure));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    assert!(owner.operate(OperatorAction::Start).is_err());
    drop(owner);
    off(&source);
}

#[test]
fn raw_byte_budget_blocks_start_and_count_budget_stops_drive() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (mut byte_owner, _writer) = session(epoch);
    let text = "x".repeat(16_000);
    let item = Event::Dispatch {
        source: "fixture",
        command: Cow::Borrowed(&text),
        status: "sent",
        at_ns: 0,
    };
    for _ in 0..514 {
        byte_owner
            .accept(Correlation::Run, std::slice::from_ref(&item))
            .unwrap();
    }
    let mut owner = RecordingLoop::new(&source, &thermometer, byte_owner, epoch);
    owner.recover().unwrap();
    owner.operate(OperatorAction::Arm).unwrap();
    assert!((0..100).any(|_| owner.tick(Instant::now()).is_err()));
    assert!(owner.operate(OperatorAction::Start).is_err());
    assert_eq!(owner.poll().failure, Some(Failure::QueueBytes));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    drop(owner);
    off(&source);

    let epoch = Instant::now();
    let (count_owner, _writer) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, count_owner, epoch);
    start(&mut owner);
    let reached = (0..100).any(|_| owner.tick(Instant::now()).is_err());
    assert!(reached);
    assert_eq!(owner.poll().failure, Some(Failure::QueueRecords));
    assert_eq!(owner.fault_reason(), Some(FaultReason::JournalFailure));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    drop(owner);
    off(&source);
}

#[test]
fn failed_first_branch_withholds_second_and_reconnect_requires_operator() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (session, _writer) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    start(&mut owner);
    source.override_next_query("print(smua.source.leveli)", b"0.0\n");
    assert!(owner.tick(Instant::now()).is_err());
    assert_eq!(reported(&source, "print(smub.source.leveli)"), 0.0);
    assert_eq!(owner.output_state(), OutputState::Unknown);
    assert!(owner.fallback_requested());
    owner.recover().unwrap();
    assert_eq!(owner.state(), MachineState::FaultLatched);
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    owner.operate(OperatorAction::Reset).unwrap();
    assert!(owner.operate(OperatorAction::Start).is_err());
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
    assert_eq!(owner.state(), MachineState::Running);
    owner.operate(OperatorAction::Stop).unwrap();
    drop(owner);
    off(&source);
}

#[test]
fn stop_does_not_wait_for_a_writer_that_never_drains() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (session, _stalled) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    start(&mut owner);
    owner.tick(Instant::now()).unwrap();
    owner.begin_close().unwrap();
    assert_eq!(owner.poll_close(), CloseResult::Pending);
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    drop(owner);
    off(&source);
}

#[test]
fn incomplete_start_batch_never_dispatches_either_on() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (initial, _initial_writer) = session(epoch);
    let mut calibration = RecordingLoop::new(&source, &thermometer, initial, epoch);
    calibration.recover().unwrap();
    calibration.operate(OperatorAction::Arm).unwrap();
    let setup_records = calibration.poll().queued_records;
    drop(calibration);

    let epoch = Instant::now();
    let (mut bounded, _writer) = session(epoch);
    let filler = Event::Operator {
        action: "fixture",
        at_ns: 0,
    };
    let reserved = cvtl_store::session::QUEUE_RECORDS - usize::try_from(setup_records).unwrap() - 1;
    bounded
        .accept(Correlation::Run, &vec![filler; reserved])
        .unwrap();
    let mut owner = RecordingLoop::new(&source, &thermometer, bounded, epoch);
    owner.recover().unwrap();
    owner.operate(OperatorAction::Arm).unwrap();
    assert_eq!(owner.poll().queued_records, 1023);
    let readbacks = [
        source.query_count("print(smua.source.output)"),
        source.query_count("print(smub.source.output)"),
    ];
    assert!(matches!(
        owner.operate(OperatorAction::Start),
        Err(cvtl_sim::recording::Error::Journal(Failure::QueueRecords))
    ));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    assert_eq!(owner.poll().queued_records, 1023);
    // Each increment belongs to the OFF pair. An ON would add a readback.
    assert_eq!(
        source.query_count("print(smua.source.output)"),
        readbacks[0] + 1
    );
    assert_eq!(
        source.query_count("print(smub.source.output)"),
        readbacks[1] + 1
    );
}

#[test]
fn stop_then_start_preloads_zero_before_on() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (session, _writer) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    start(&mut owner);
    assert!(owner
        .tick(Instant::now())
        .unwrap()
        .command_latency
        .is_some());
    owner.operate(OperatorAction::Stop).unwrap();
    owner.operate(OperatorAction::Arm).unwrap();
    owner.operate(OperatorAction::Start).unwrap();
    assert_eq!(owner.state(), MachineState::Running);
    owner.operate(OperatorAction::Stop).unwrap();
    drop(owner);
    assert_eq!(reported(&source, "print(smua.source.leveli)"), 0.0);
    assert_eq!(reported(&source, "print(smub.source.leveli)"), 0.0);
}

#[test]
fn acquired_link_loss_keeps_m5_priority_when_input_batch_cannot_fit() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (reference, _reference_writer) = session(epoch);
    let mut calibration = RecordingLoop::new(&source, &thermometer, reference, epoch);
    start(&mut calibration);
    let startup = calibration.poll().queued_records as usize;
    drop(calibration);

    let epoch = Instant::now();
    let (mut bounded, _writer) = session(epoch);
    let filler = Event::Operator {
        action: "fixture",
        at_ns: 0,
    };
    let reserved = cvtl_store::session::QUEUE_RECORDS - startup - 1;
    bounded
        .accept(Correlation::Run, &vec![filler; reserved])
        .unwrap();
    let mut owner = RecordingLoop::new(&source, &thermometer, bounded, epoch);
    start(&mut owner);
    assert_eq!(owner.poll().queued_records, 1023);
    source.disconnect_next_query();
    assert!(matches!(
        owner.tick(Instant::now()),
        Err(cvtl_sim::recording::Error::Journal(Failure::QueueRecords))
    ));
    assert_eq!(owner.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert_eq!(owner.output_state(), OutputState::Unknown);
    assert!(owner.fallback_requested());
}

#[test]
fn duplicate_tick_does_not_leave_previous_drive_on() {
    let source = KeithleySimulator::start().unwrap();
    let thermometer = LakeShoreSim::spawn().unwrap();
    let epoch = Instant::now();
    let (session, _writer) = session(epoch);
    let mut owner = RecordingLoop::new(&source, &thermometer, session, epoch);
    start(&mut owner);
    let due = Instant::now();
    owner.tick(due).unwrap();
    assert!(matches!(
        owner.tick(due),
        Err(cvtl_sim::recording::Error::StaleTick)
    ));
    assert_eq!(owner.output_state(), OutputState::OffVerified);
    drop(owner);
    off(&source);
}
