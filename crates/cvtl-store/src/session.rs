// FILE: crates/cvtl-store/src/session.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Own journal admission and published health
//   SCOPE: SPSC handoff, quotas, deadlines, and terminal close
//   DEPENDS: M-CVTL-SAFETY, rtrb
//   LINKS: M-CVTL-STORE
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Session - one producer and authoritative frontiers
//   WriterPort - consumer and feedback boundary
//   Status - coherent health snapshot
//   Receipt - accepted range for one correlation
//   CloseResult - terminal result after bounded close
// END_MODULE_MAP
//! Single owner of admission, byte/record budgets, and published frontiers.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cvtl_safety::{
    machine::MachineState,
    monitor::Warning,
    supervisor::{
        ControlFault, Equivalence, JournalError, OperatorAction, SafetyJournal, SafetyRecord,
    },
    Branch, FaultReason,
};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::record::{
    encode_line, valid_run, Channel, Correlation, Event, Issue, Record, RecordError,
    MAX_SAFETY_ISSUES,
};

pub const QUEUE_RECORDS: usize = 1024;
pub const QUEUE_BYTES: usize = 8 * 1024 * 1024;
const LEDGER_CAPACITY: usize = 2 * QUEUE_RECORDS;
const FEEDBACK_CAPACITY: usize = 3 * LEDGER_CAPACITY + 2;
const PROGRESS_LIMIT: Duration = Duration::from_secs(2);
const AGE_LIMIT: Duration = Duration::from_secs(2);
const CLOSE_LIMIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WriterFault {
    Open = 1,
    Write,
    Flush,
    Sync,
    Panic,
    Disconnected,
    Feedback,
}

impl WriterFault {
    fn from_code(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Open,
            2 => Self::Write,
            3 => Self::Flush,
            4 => Self::Sync,
            5 => Self::Panic,
            6 => Self::Disconnected,
            7 => Self::Feedback,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    NotReady,
    Closing,
    Record(RecordError),
    QueueRecords,
    QueueBytes,
    Ledger,
    Protocol,
    Clock,
    ProgressDeadline,
    AgeDeadline,
    Writer(WriterFault),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Журнал: {self:?}")
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub generation: u64,
    pub correlation: Correlation,
    pub first_seq: u64,
    pub last_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completed {
    pub generation: u64,
    pub last_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseResult {
    Pending,
    Completed(Completed),
    Failed(Failure),
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Expired {
    pub first_seq: u64,
    pub last_seq: u64,
    pub first_accepted: Duration,
    pub observed_at: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub ready: bool,
    pub accepted: u64,
    pub written: u64,
    pub durable: u64,
    pub queued_records: u64,
    pub queued_bytes: u64,
    pub failure: Option<Failure>,
    pub writer_fault: Option<WriterFault>,
    pub progress_overdue: bool,
    pub expired: Option<Expired>,
    pub events_failed: bool,
}

pub struct Line {
    bytes: Box<[u8]>,
    operator_view: bool,
}
impl Line {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn in_operator_view(&self) -> bool {
        self.operator_view
    }
}

pub struct Batch {
    lines: Vec<Line>,
    bytes: u64,
    last_seq: u64,
    queued_at: Instant,
}
impl Batch {
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub(crate) fn queue_latency(&self) -> Duration {
        self.queued_at.elapsed()
    }
}

#[derive(Clone, Copy, Debug)]
enum Update {
    Ready,
    Dequeued { records: u64, bytes: u64 },
    Written(u64),
    Synced(u64),
    Finished(u64),
}

#[derive(Clone, Copy)]
struct Feedback {
    generation: u64,
    update: Update,
}

#[derive(Default)]
struct Shared {
    closing: AtomicBool,
    fault: AtomicU8,
    events_failed: AtomicBool,
}

type AcceptedObserver = dyn FnMut(Receipt, &[Event<'_>]);

pub struct Session {
    run: String,
    generation: u64,
    clock: Box<dyn Fn() -> Duration>,
    producer: Producer<Batch>,
    feedback: Consumer<Feedback>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    accepted_observer: Option<Box<AcceptedObserver>>,
    opened: bool,
    accepted: u64,
    accepted_bytes: u64,
    dequeued: u64,
    dequeued_bytes: u64,
    written: u64,
    durable: u64,
    ledger: VecDeque<Duration>,
    last_clock: Option<Duration>,
    progress_at: Option<Duration>,
    progress_overdue: bool,
    expired: Option<Expired>,
    failure: Option<Failure>,
    close_started: Option<Duration>,
    close_result: CloseResult,
}

impl Session {
    /// File I/O belongs to the returned endpoint. The caller owns this side
    /// and supplies its monotonic clock.
    pub fn channel(
        run: &str,
        generation: u64,
        clock: impl Fn() -> Duration + 'static,
    ) -> Result<(Self, WriterPort), Failure> {
        if !valid_run(run) || generation == 0 {
            return Err(Failure::Record(RecordError::Format));
        }
        let (producer, consumer) = RingBuffer::new(QUEUE_RECORDS);
        let (back, feedback) = RingBuffer::new(FEEDBACK_CAPACITY);
        let shared = Arc::new(Shared::default());
        let writer = WriterPort {
            generation,
            consumer,
            back,
            shared: shared.clone(),
            dequeued: 0,
            dequeued_bytes: 0,
            orderly: false,
        };
        let owner = Self {
            run: run.into(),
            generation,
            clock: Box::new(clock),
            producer,
            feedback,
            shared,
            worker: None,
            accepted_observer: None,
            opened: false,
            accepted: 0,
            accepted_bytes: 0,
            dequeued: 0,
            dequeued_bytes: 0,
            written: 0,
            durable: 0,
            ledger: VecDeque::with_capacity(LEDGER_CAPACITY),
            last_clock: None,
            progress_at: None,
            progress_overdue: false,
            expired: None,
            failure: None,
            close_started: None,
            close_result: CloseResult::Pending,
        };
        Ok((owner, writer))
    }

    pub fn poll(&mut self) -> Status {
        while let Ok(feedback) = self.feedback.pop() {
            if feedback.generation != self.generation {
                continue;
            }
            let (now, writer_fault) = self.observe_time();
            // Evaluate the previous obligations before publishing either frontier.
            self.check_deadlines(now);
            self.check_close(now, writer_fault);
            match feedback.update {
                Update::Ready if !self.opened => self.opened = true,
                Update::Dequeued { records, bytes }
                    if (self.dequeued..=self.accepted).contains(&records)
                        && (self.dequeued_bytes..=self.accepted_bytes).contains(&bytes) =>
                {
                    self.dequeued = records;
                    self.dequeued_bytes = bytes;
                }
                Update::Written(seq) if (self.written..=self.dequeued).contains(&seq) => {
                    if seq > self.written {
                        self.progress_at = Some(now);
                    }
                    self.written = seq;
                }
                Update::Synced(seq) if (self.durable..=self.written).contains(&seq) => {
                    let retired = (seq - self.durable) as usize;
                    self.ledger.drain(..retired);
                    self.durable = seq;
                    if retired > 0 {
                        self.progress_at = (!self.ledger.is_empty()).then_some(now);
                    }
                }
                Update::Finished(seq)
                    if self.close_started.is_some()
                        && seq == self.durable
                        && seq == self.accepted =>
                {
                    if self.close_result == CloseResult::Pending {
                        self.close_result = match self.failure {
                            Some(failure) => CloseResult::Failed(failure),
                            None => CloseResult::Completed(Completed {
                                generation: self.generation,
                                last_seq: seq,
                            }),
                        };
                    }
                }
                _ => {
                    self.fail(Failure::Protocol);
                }
            }
        }
        let (now, writer_fault) = self.observe_time();
        self.check_deadlines(now);
        self.check_close(now, writer_fault);
        self.status(writer_fault)
    }

    fn writer_fault(&self) -> Option<WriterFault> {
        WriterFault::from_code(self.shared.fault.load(Ordering::Acquire))
    }

    fn observe_time(&mut self) -> (Duration, Option<WriterFault>) {
        let writer_fault = self.writer_fault();
        if let Some(fault) = writer_fault {
            self.fail(Failure::Writer(fault));
        }
        let now = (self.clock)();
        if let Some(previous) = self.last_clock.filter(|previous| now < *previous) {
            self.fail(Failure::Clock);
            return (previous, writer_fault);
        }
        self.last_clock = Some(now);
        (now, writer_fault)
    }

    fn check_deadlines(&mut self, now: Duration) {
        if self.ledger.is_empty() {
            return;
        }
        if self
            .progress_at
            .is_some_and(|last| now - last >= PROGRESS_LIMIT)
        {
            self.progress_overdue = true;
            self.fail(Failure::ProgressDeadline);
        }
        if let Some(&first) = self
            .ledger
            .front()
            .filter(|first| now - **first >= AGE_LIMIT)
        {
            // Acceptance times are ordered. The overdue entries are a prefix.
            // Preserve the first range even after later acknowledgements retire it.
            if self.expired.is_none() {
                let overdue = self
                    .ledger
                    .iter()
                    .take_while(|at| now - **at >= AGE_LIMIT)
                    .count();
                self.expired = Some(Expired {
                    first_seq: self.durable + 1,
                    last_seq: self.durable + overdue as u64,
                    first_accepted: first,
                    observed_at: now,
                });
            }
            self.fail(Failure::AgeDeadline);
        }
    }

    fn status(&self, writer_fault: Option<WriterFault>) -> Status {
        Status {
            ready: self.opened && self.close_started.is_none() && self.failure.is_none(),
            accepted: self.accepted,
            written: self.written,
            durable: self.durable,
            queued_records: self.accepted - self.dequeued,
            queued_bytes: self.accepted_bytes - self.dequeued_bytes,
            failure: self.failure,
            writer_fault,
            progress_overdue: self.progress_overdue,
            expired: self.expired,
            events_failed: self.shared.events_failed.load(Ordering::Acquire),
        }
    }

    pub fn accept(
        &mut self,
        correlation: Correlation,
        events: &[Event<'_>],
    ) -> Result<Receipt, Failure> {
        let status = self.poll();
        if self.close_started.is_some() {
            return Err(Failure::Closing);
        }
        if let Some(failure) = status.failure {
            return Err(failure);
        }
        if !status.ready {
            return Err(Failure::NotReady);
        }
        if self.producer.is_abandoned() {
            return Err(self.fail(Failure::Writer(WriterFault::Disconnected)));
        }
        let receipt = self
            .push(correlation, events)
            .map_err(|failure| self.fail(failure))?;
        if let Some(observer) = &mut self.accepted_observer {
            observer(receipt, events);
        }
        Ok(receipt)
    }

    /// Install the owner's synchronous observer for admitted batches.
    /// The observer must do bounded work in memory without waiting or doing I/O.
    pub fn observe_accepted(&mut self, observer: impl FnMut(Receipt, &[Event<'_>]) + 'static) {
        self.accepted_observer = Some(Box::new(observer));
    }

    fn push(&mut self, correlation: Correlation, events: &[Event<'_>]) -> Result<Receipt, Failure> {
        let count = events.len();
        if count == 0 {
            return Err(Failure::Record(RecordError::Format));
        }
        if count as u64 > QUEUE_RECORDS as u64 - (self.accepted - self.dequeued) {
            return Err(Failure::QueueRecords);
        }
        if count > LEDGER_CAPACITY - self.ledger.len() {
            return Err(Failure::Ledger);
        }
        // The owner's stamp precedes bounded serialization, so any preemption
        // during admission counts conservatively against age.
        let (at, _) = self.observe_time();
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let accepted_ns = nanos(at).ok_or(Failure::Clock)?;
        let first_seq = self.accepted.checked_add(1).ok_or(Failure::Protocol)?;
        let last_seq = self
            .accepted
            .checked_add(count as u64)
            .ok_or(Failure::Protocol)?;
        let byte_budget = QUEUE_BYTES as u64 - (self.accepted_bytes - self.dequeued_bytes);
        let mut lines = Vec::with_capacity(count);
        let mut bytes = 0_u64;
        for (index, event) in events.iter().enumerate() {
            event.validate().map_err(Failure::Record)?;
            let record = Record::new(
                &self.run,
                self.generation,
                first_seq + index as u64,
                accepted_ns,
                correlation,
                event.clone(),
            );
            let encoded = encode_line(&record).map_err(Failure::Record)?;
            bytes += encoded.len() as u64;
            if bytes > byte_budget {
                return Err(Failure::QueueBytes);
            }
            lines.push(Line {
                bytes: encoded.into_boxed_slice(),
                operator_view: event.in_operator_view(),
            });
        }
        let accepted_bytes = self
            .accepted_bytes
            .checked_add(bytes)
            .ok_or(Failure::Protocol)?;
        if let Some(failure) = self.poll().failure {
            return Err(failure);
        }
        if self.last_clock.is_some_and(|now| now - at >= AGE_LIMIT) {
            return Err(Failure::AgeDeadline);
        }
        self.producer
            .push(Batch {
                lines,
                bytes,
                last_seq,
                queued_at: Instant::now(),
            })
            .map_err(|_| Failure::QueueRecords)?;
        self.accepted = last_seq;
        self.accepted_bytes = accepted_bytes;
        if self.ledger.is_empty() {
            self.progress_at = Some(at);
        }
        self.ledger.extend(std::iter::repeat_n(at, count));
        self.wake();
        Ok(Receipt {
            generation: self.generation,
            correlation,
            first_seq,
            last_seq,
        })
    }

    fn fail(&mut self, reason: Failure) -> Failure {
        *self.failure.get_or_insert(reason)
    }

    pub fn journal(&mut self, correlation: Correlation) -> Journal<'_> {
        Journal {
            session: self,
            correlation,
            targets_w: None,
        }
    }

    pub fn begin_close(&mut self) {
        if self.close_started.is_none() {
            self.close_started = Some(self.observe_time().0);
            self.signal_close();
        }
    }

    pub fn poll_close(&mut self) -> CloseResult {
        self.poll();
        self.close_result
    }

    fn check_close(&mut self, now: Duration, writer_fault: Option<WriterFault>) {
        if let Some(started) = self
            .close_started
            .filter(|_| self.close_result == CloseResult::Pending)
        {
            if let Some(failure) = writer_fault.map(Failure::Writer).or_else(|| {
                self.failure
                    .filter(|reason| matches!(reason, Failure::Clock | Failure::Protocol))
            }) {
                self.close_result = CloseResult::Failed(failure);
            } else if now - started >= CLOSE_LIMIT {
                self.close_result = CloseResult::TimedOut;
            }
        }
    }

    fn signal_close(&self) {
        self.shared.closing.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }

    pub(crate) fn attach_worker(&mut self, worker: JoinHandle<()>) {
        self.worker = Some(worker);
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.signal_close();
    }
}

pub struct WriterPort {
    generation: u64,
    consumer: Consumer<Batch>,
    back: Producer<Feedback>,
    shared: Arc<Shared>,
    dequeued: u64,
    dequeued_bytes: u64,
    orderly: bool,
}

impl WriterPort {
    pub fn ready(&mut self) -> Result<(), WriterFault> {
        self.send(Update::Ready)
    }

    pub fn next_batch(&mut self) -> Result<Option<Batch>, WriterFault> {
        let Ok(batch) = self.consumer.pop() else {
            return Ok(None);
        };
        self.dequeued = batch.last_seq;
        self.dequeued_bytes += batch.bytes;
        self.send(Update::Dequeued {
            records: self.dequeued,
            bytes: self.dequeued_bytes,
        })?;
        Ok(Some(batch))
    }

    pub fn written(&mut self, seq: u64) -> Result<(), WriterFault> {
        self.send(Update::Written(seq))
    }
    pub fn synced(&mut self, seq: u64) -> Result<(), WriterFault> {
        self.send(Update::Synced(seq))
    }

    pub fn finish(&mut self, seq: u64) -> Result<(), WriterFault> {
        self.send(Update::Finished(seq))?;
        self.orderly = true;
        Ok(())
    }

    pub fn closing(&self) -> bool {
        self.shared.closing.load(Ordering::Acquire)
    }

    pub fn fail(&self, failure: WriterFault) {
        let _ = self.shared.fault.compare_exchange(
            0,
            failure as u8,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn events_failed(&self) {
        self.shared.events_failed.store(true, Ordering::Release);
    }

    fn send(&mut self, update: Update) -> Result<(), WriterFault> {
        self.back
            .push(Feedback {
                generation: self.generation,
                update,
            })
            .map_err(|_| {
                self.fail(WriterFault::Feedback);
                WriterFault::Feedback
            })
    }
}

impl Drop for WriterPort {
    fn drop(&mut self) {
        if !self.orderly {
            self.fail(if thread::panicking() {
                WriterFault::Panic
            } else {
                WriterFault::Disconnected
            });
        }
    }
}

pub struct Journal<'a> {
    session: &'a mut Session,
    correlation: Correlation,
    targets_w: Option<[f64; 2]>,
}

impl Journal<'_> {
    pub fn targets_w(&self) -> Option<[f64; 2]> {
        self.targets_w
    }
}

impl SafetyJournal for Journal<'_> {
    fn record(&mut self, record: SafetyRecord<'_>) -> Result<(), JournalError> {
        let event = match journal_event(record) {
            Ok(event) => event,
            Err(error) => {
                self.session.fail(Failure::Record(error));
                return Err(JournalError);
            }
        };
        let targets = match &event {
            Event::Control { targets_w, .. } => *targets_w,
            _ => None,
        };
        self.session
            .accept(self.correlation, &[event])
            .map_err(|_| JournalError)?;
        self.targets_w = targets;
        Ok(())
    }
}

fn journal_event(record: SafetyRecord<'_>) -> Result<Event<'static>, RecordError> {
    let time = |at| nanos(at).ok_or(RecordError::Format);
    Ok(match record {
        SafetyRecord::Observation {
            at,
            state,
            faults,
            warnings,
        } => {
            if faults.len() > MAX_SAFETY_ISSUES || warnings.len() > MAX_SAFETY_ISSUES {
                return Err(RecordError::Size);
            }
            Event::Observation {
                at_ns: time(at)?,
                state: match state {
                    MachineState::Disconnected => "disconnected",
                    MachineState::Observing => "observing",
                    MachineState::OutputOffVerified => "output_off_verified",
                    MachineState::Armed => "armed",
                    MachineState::Running => "running",
                    MachineState::Stopping => "stopping",
                    MachineState::FaultLatched => "fault_latched",
                },
                faults: faults.iter().copied().map(fault_issue).collect(),
                warnings: warnings
                    .iter()
                    .map(|warning| match warning {
                        Warning::HeadApproaching => "head_approaching",
                        Warning::MeasurementStale => "measurement_stale",
                    })
                    .collect(),
            }
        }
        SafetyRecord::Operator { at, action } => Event::Operator {
            at_ns: time(at)?,
            action: match action {
                OperatorAction::Arm => "arm",
                OperatorAction::Start => "start",
                OperatorAction::Stop => "stop",
                OperatorAction::Reset => "reset",
            },
        },
        SafetyRecord::Control {
            at,
            warning,
            fault,
            output,
        } => Event::Control {
            at_ns: time(at)?,
            requested_w: output.requested.map(|watts| watts.get()),
            limited_w: Some(output.limited.get()),
            measured_w: output.measured.map(|watts| watts.get()),
            targets_w: Some(output.targets.map(|watts| watts.get())),
            equivalence: Some(match output.equivalence {
                Equivalence::Inactive => "inactive",
                Equivalence::Inhibited => "inhibited",
                Equivalence::Fault => "fault",
                Equivalence::Limited => "limited",
                Equivalence::Settling => "settling",
                Equivalence::WithinTolerance => "within_tolerance",
            }),
            slew_limited: output.slew_limited,
            power_limited: output.power_limited,
            current_limited: output.current_limited,
            warning: warning.map(|_| "measurement_stale"),
            fault: fault.map(|fault| match fault {
                ControlFault::BranchLost => "branch_lost",
                ControlFault::MeasurementUnavailable => "measurement_unavailable",
                ControlFault::MeasurementStale => "measurement_stale",
                ControlFault::TemperatureInterlock => "temperature_interlock",
                ControlFault::Clock => "clock",
                ControlFault::Calculation => "calculation",
            }),
        },
    })
}

pub fn fault_issue(fault: FaultReason) -> Issue<'static> {
    let (code, branch) = match fault {
        FaultReason::BranchLost => ("branch_lost", None),
        FaultReason::SourceLinkLost => ("source_link_lost", None),
        FaultReason::ThermometerLinkLost => ("thermometer_link_lost", None),
        FaultReason::MeasurementUnavailable => ("measurement_unavailable", None),
        FaultReason::PowerMeasurementStale => ("power_measurement_stale", None),
        FaultReason::TemperatureStale => ("temperature_stale", None),
        FaultReason::BranchCurrent(branch) => ("branch_current", Some(branch)),
        FaultReason::BranchVoltage(branch) => ("branch_voltage", Some(branch)),
        FaultReason::BranchPower(branch) => ("branch_power", Some(branch)),
        FaultReason::TotalPower => ("total_power", None),
        FaultReason::MissedTick => ("missed_tick", None),
        FaultReason::InstrumentQueue => ("instrument_queue", None),
        FaultReason::ModelFailure => ("model_failure", None),
        FaultReason::JournalFailure => ("journal_failure", None),
        FaultReason::MonotonicClock => ("monotonic_clock", None),
        FaultReason::WallClockJump => ("wall_clock_jump", None),
        FaultReason::ChannelMismatch => ("channel_mismatch", None),
        FaultReason::PowerMismatch(branch) => ("power_mismatch", Some(branch)),
        FaultReason::HeadInversion => ("head_inversion", None),
        FaultReason::HeadAbsoluteTemperature => ("head_absolute_temperature", None),
        FaultReason::HeadRate => ("head_rate", None),
        FaultReason::HeadPower => ("head_power", None),
        FaultReason::OutputUnconfirmed => ("output_unconfirmed", None),
        FaultReason::ControllerFailure => ("controller_failure", None),
    };
    Issue {
        code,
        channel: branch.map(|branch| match branch {
            Branch::A => Channel::A,
            Branch::B => Channel::B,
        }),
    }
}

fn nanos(duration: Duration) -> Option<u64> {
    duration.as_nanos().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn observer_sees_only_accepted_batches_and_their_receipts() {
        let (mut session, mut writer) = Session::channel("run", 4, Duration::default).unwrap();
        let observed = Rc::new(RefCell::new(Vec::new()));
        let captures = Rc::clone(&observed);
        session.observe_accepted(move |receipt, events| {
            let actions = events
                .iter()
                .map(|event| match event {
                    Event::Operator { action, .. } => (*action).to_owned(),
                    _ => panic!("expected operator event"),
                })
                .collect::<Vec<_>>();
            captures.borrow_mut().push((receipt, actions));
        });
        let name = String::from("arm");
        let arm = Event::Operator {
            action: &name,
            at_ns: 0,
        };
        assert_eq!(
            session.accept(Correlation::Request(3), std::slice::from_ref(&arm)),
            Err(Failure::NotReady)
        );
        assert!(observed.borrow().is_empty());
        writer.ready().unwrap();
        let correlation = Correlation::Request(5);
        let receipt = session
            .accept(
                correlation,
                &[
                    arm,
                    Event::Operator {
                        action: "start",
                        at_ns: 1,
                    },
                ],
            )
            .unwrap();
        assert_eq!(
            receipt,
            Receipt {
                generation: 4,
                correlation,
                first_seq: 1,
                last_seq: 2,
            }
        );
        assert_eq!(
            *observed.borrow(),
            vec![(receipt, vec!["arm".into(), "start".into()])]
        );
        assert_eq!(writer.next_batch().unwrap().unwrap().last_seq(), 2);
        assert!(matches!(
            session.accept(
                Correlation::Request(6),
                &[
                    Event::Operator {
                        action: "stop",
                        at_ns: 2,
                    },
                    Event::Parameters {
                        mass_kg: Some(f64::NAN),
                        material: None,
                        bridge: None,
                        dh_event_j: None,
                        at_ns: 2,
                    },
                ],
            ),
            Err(Failure::Record(_))
        ));
        assert_eq!(observed.borrow().len(), 1);
        assert!(writer.next_batch().unwrap().is_none());
    }
}
