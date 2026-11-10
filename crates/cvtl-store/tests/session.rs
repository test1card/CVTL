use std::{borrow::Cow, cell::Cell, rc::Rc, time::Duration};

use cvtl_safety::supervisor::{OperatorAction, SafetyJournal, SafetyRecord};
use cvtl_store::{
    record::{decode_line, Correlation, Event},
    session::{Failure, Session, WriterPort},
};

fn fixture() -> (Rc<Cell<Duration>>, Session, WriterPort) {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let source = clock.clone();
    let (mut session, mut writer) = Session::channel("run", 1, move || source.get()).unwrap();
    writer.ready().unwrap();
    assert!(session.poll().ready);
    (clock, session, writer)
}

fn operator() -> Event<'static> {
    Event::Operator {
        action: "arm",
        at_ns: 0,
    }
}

#[test]
fn admission_rejects_whole_batch_without_consuming_sequence() {
    let (_, mut session, mut writer) = fixture();
    let invalid = Event::Parameters {
        mass_kg: Some(f64::NAN),
        material: None,
        bridge: None,
        dh_event_j: None,
        at_ns: 0,
    };
    let before = session.accept(Correlation::Tick(2), &[operator()]).unwrap();
    assert_eq!(
        (before.first_seq, before.last_seq, before.generation),
        (1, 1, 1)
    );
    assert!(matches!(
        session.accept(Correlation::Tick(3), &[operator(), invalid]),
        Err(Failure::Record(_))
    ));
    assert_eq!(session.poll().accepted, 1);
    let batch = writer.next_batch().unwrap().unwrap();
    assert_eq!(batch.last_seq(), 1);
    assert_eq!(batch.lines().len(), 1);
    assert_eq!(decode_line(batch.lines()[0].bytes()).unwrap().seq, 1);
    assert!(writer.next_batch().unwrap().is_none());
}

#[test]
fn admission_enforces_both_queue_limits_independently() {
    let (_, mut session, _writer) = fixture();
    let small = vec![operator(); 1024];
    session.accept(Correlation::Run, &small).unwrap();
    assert_eq!(
        session.accept(Correlation::Run, &[operator()]),
        Err(Failure::QueueRecords)
    );
    assert_eq!(session.poll().accepted, 1024);

    let (_, mut session, _writer) = fixture();
    let payload = "x".repeat(16_000);
    let event = Event::Dispatch {
        source: "synthetic",
        command: Cow::Borrowed(&payload),
        status: "sent",
        at_ns: 0,
    };
    for _ in 0..530 {
        if let Err(failure) = session.accept(Correlation::Run, std::slice::from_ref(&event)) {
            assert_eq!(failure, Failure::QueueBytes);
            assert!((500..530).contains(&session.poll().accepted));
            return;
        }
    }
    panic!("queue byte limit did not trip");
}

#[test]
fn admission_dequeue_releases_quota_but_not_deadline_ledger() {
    let (_, mut session, mut writer) = fixture();
    let records = vec![operator(); 1024];
    for batch_number in 1..=2 {
        let receipt = session.accept(Correlation::Run, &records).unwrap();
        assert_eq!(receipt.last_seq, batch_number * 1024);
        let batch = writer.next_batch().unwrap().unwrap();
        writer.written(batch.last_seq()).unwrap();
        let status = session.poll();
        assert_eq!(status.queued_records, 0);
        assert_eq!(status.durable, 0);
    }
    assert_eq!(
        session.accept(Correlation::Run, &[operator()]),
        Err(Failure::Ledger)
    );
}

#[test]
fn admission_safety_journal_publishes_owned_record_and_rejects_after_close() {
    let (_, mut session, mut writer) = fixture();
    let mut journal = session.journal(Correlation::Request(8));
    journal
        .record(SafetyRecord::Operator {
            at: Duration::from_millis(3),
            action: OperatorAction::Arm,
        })
        .unwrap();
    let batch = writer.next_batch().unwrap().unwrap();
    let record = decode_line(batch.lines()[0].bytes()).unwrap();
    assert_eq!(record.correlation, Correlation::Request(8));
    assert_eq!(
        record.event,
        Event::Operator {
            action: "arm",
            at_ns: 3_000_000
        }
    );
    session.begin_close();
    assert!(writer.closing());
    assert_eq!(
        session.accept(Correlation::Run, &[operator()]),
        Err(Failure::Closing)
    );
}

#[test]
fn health_deadline_is_strict_on_both_observation_orders() {
    for (millis, sample_first, expected) in [
        (1999, false, None),
        (2000, false, Some(Failure::ProgressDeadline)),
        (2000, true, Some(Failure::ProgressDeadline)),
        (2050, false, Some(Failure::ProgressDeadline)),
    ] {
        let (clock, mut session, mut writer) = fixture();
        session.accept(Correlation::Tick(1), &[operator()]).unwrap();
        let seq = writer.next_batch().unwrap().unwrap().last_seq();
        clock.set(Duration::from_millis(millis));
        if sample_first {
            assert_eq!(session.poll().failure, expected);
        }
        writer.written(seq).unwrap();
        writer.synced(seq).unwrap();
        let status = session.poll();
        assert_eq!(status.failure, expected);
        assert_eq!(status.durable, 1);
        assert_eq!(status.expired.is_some(), expected.is_some());
    }
}

#[test]
fn health_one_ack_keeps_the_expired_prefix_and_a_timely_record() {
    let (clock, mut session, mut writer) = fixture();
    session.accept(Correlation::Tick(1), &[operator()]).unwrap();
    writer.next_batch().unwrap().unwrap();
    clock.set(Duration::from_millis(100));
    session.accept(Correlation::Tick(2), &[operator()]).unwrap();
    writer.next_batch().unwrap().unwrap();
    clock.set(Duration::from_millis(1900));
    writer.written(2).unwrap();
    assert_eq!(session.poll().failure, None);
    clock.set(Duration::from_millis(2050));
    writer.synced(2).unwrap();
    let status = session.poll();
    assert_eq!(status.durable, 2);
    assert_eq!(status.failure, Some(Failure::AgeDeadline));
    let expired = status.expired.unwrap();
    assert_eq!((expired.first_seq, expired.last_seq), (1, 1));
    assert_eq!(expired.first_accepted, Duration::ZERO);
    assert_eq!(expired.observed_at, Duration::from_millis(2050));
    assert_eq!(
        session.accept(Correlation::Tick(3), &[operator()]),
        Err(Failure::AgeDeadline)
    );
}

#[test]
fn health_empty_queue_and_returned_sync_do_not_publish_progress() {
    let (clock, mut session, mut writer) = fixture();
    clock.set(Duration::from_secs(10));
    assert_eq!(session.poll().failure, None);
    session.accept(Correlation::Tick(1), &[operator()]).unwrap();
    writer.next_batch().unwrap().unwrap();
    clock.set(Duration::from_millis(10_250));
    writer.written(1).unwrap();
    assert_eq!(session.poll().queued_records, 0);
    clock.set(Duration::from_millis(11_900));
    writer.synced(1).unwrap();
    // The writer has returned. Publication is owned by the control thread.
    clock.set(Duration::from_secs(12));
    let status = session.poll();
    assert_eq!(status.durable, 1);
    assert_eq!(status.failure, Some(Failure::AgeDeadline));
}

#[test]
fn health_dequeue_and_new_records_do_not_reset_progress() {
    let (clock, mut session, mut writer) = fixture();
    session.accept(Correlation::Tick(1), &[operator()]).unwrap();
    clock.set(Duration::from_millis(1900));
    writer.next_batch().unwrap().unwrap();
    session.accept(Correlation::Tick(2), &[operator()]).unwrap();
    clock.set(Duration::from_secs(2));
    let status = session.poll();
    assert_eq!(status.failure, Some(Failure::ProgressDeadline));
    assert_eq!(status.expired.unwrap().first_seq, 1);
}

#[test]
fn health_writer_failure_after_record_has_priority_and_survives_late_ack() {
    use cvtl_store::session::WriterFault;
    let (clock, mut session, mut writer) = fixture();
    session
        .journal(Correlation::Request(4))
        .record(SafetyRecord::Operator {
            at: Duration::ZERO,
            action: OperatorAction::Start,
        })
        .unwrap();
    writer.next_batch().unwrap().unwrap();
    writer.written(1).unwrap();
    clock.set(Duration::from_secs(2));
    writer.fail(WriterFault::Sync);
    assert_eq!(
        session.poll().failure,
        Some(Failure::Writer(WriterFault::Sync))
    );
    writer.synced(1).unwrap();
    let status = session.poll();
    assert_eq!(status.failure, Some(Failure::Writer(WriterFault::Sync)));
    assert_eq!(status.writer_fault, Some(WriterFault::Sync));
    assert_eq!(status.durable, 1);
    assert!(status.progress_overdue);
    assert!(status.expired.is_some());
}

#[test]
fn health_monotonic_clock_and_session_isolation() {
    let (clock, mut session, mut writer) = fixture();
    clock.set(Duration::from_secs(1));
    session.accept(Correlation::Run, &[operator()]).unwrap();
    clock.set(Duration::ZERO);
    assert_eq!(session.poll().failure, Some(Failure::Clock));

    let (mut next, mut new_writer) = Session::channel("run", 2, Duration::default).unwrap();
    new_writer.ready().unwrap();
    let next_receipt = next.accept(Correlation::Run, &[operator()]).unwrap();
    writer.next_batch().unwrap().unwrap();
    writer.written(1).unwrap();
    writer.synced(1).unwrap();
    assert_eq!(next_receipt.generation, 2);
    assert_eq!(next.poll().durable, 0);
}

#[test]
fn health_close_reports_later_writer_error_without_replacing_first_fault() {
    use cvtl_store::session::{CloseResult, WriterFault};
    let (_, mut owner, writer) = fixture();
    owner
        .accept(Correlation::Run, &vec![operator(); 1024])
        .unwrap();
    assert_eq!(
        owner.accept(Correlation::Run, &[operator()]),
        Err(Failure::QueueRecords)
    );
    owner.begin_close();
    writer.fail(WriterFault::Sync);
    assert_eq!(
        owner.poll_close(),
        CloseResult::Failed(Failure::Writer(WriterFault::Sync))
    );
    assert_eq!(owner.poll().failure, Some(Failure::QueueRecords));
}
