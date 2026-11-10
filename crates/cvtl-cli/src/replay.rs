// FILE: crates/cvtl-cli/src/replay.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Recompute a verified recording offline
//   SCOPE: V1 logical operation, causal evidence and comparison report
//   DEPENDS: M-CVTL-STORE, M-CVTL-CONTROL, M-CVTL-SAFETY
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   run - read-only replay command
// END_MODULE_MAP
//! Recompute v1 decisions offline and compare them to the verified raw stream.

mod profile;

use std::{path::Path, process::ExitCode, time::Duration};

use cvtl_control::{pi::PowerSample, Demand, HeatPermit, Tick, TimedPowerSample};
use cvtl_core::units::{Amperes, Kelvin, Seconds, Volts, Watts};
use cvtl_safety::{
    machine::{MachineState, OffEvidence, OutputState, RequestToken},
    monitor::{BranchReading, CoolingPhase, MonitorInput, Temperatures, Warning},
    supervisor::{
        ControlFault, Effect, Equivalence, JournalError, OperatorAction, SafetyJournal,
        SafetyRecord, Supervisor,
    },
    Branch,
};
use cvtl_store::{
    record::{
        self as raw, Channel, Command, Correlation, Event, Inputs, Quality, Record, ThermalPoint,
    },
    recovery::{recover, Tail},
    session::fault_issue,
};
use serde::Serialize;

use crate::quantity;

const MAX_OPERATION_ROWS: usize = 1024;
const CHANNELS: [Channel; 2] = [Channel::A, Channel::B];

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Matched,
    Insufficient,
    Mismatch,
    IncompleteFrame,
    CorruptFrame,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    status: Status,
    seq: u64,
    correlation: Option<Correlation>,
    message: &'static str,
}
impl Diagnostic {
    fn new(
        status: Status,
        seq: u64,
        correlation: Option<Correlation>,
        message: &'static str,
    ) -> Self {
        Self {
            status,
            seq,
            correlation,
            message,
        }
    }
}

#[derive(Serialize)]
struct Report {
    run: String,
    generation: u64,
    status: Status,
    raw_verified_records: u64,
    raw_verified_frames: u64,
    raw_tail: &'static str,
    replayed_ticks: u64,
    last_replayed_tick: Option<u64>,
    historical_completion: &'static str,
    output_state_at_boundary: &'static str,
    first_diagnostic: Option<Diagnostic>,
}

pub(super) fn run(dir: &Path) -> Result<(String, ExitCode), String> {
    let profile = profile::load(dir)?;
    let mut replay = Replay {
        profile,
        operation: None,
        first: None,
        replayed: 0,
        last_tick: None,
        last_id: 0,
        verified_output: OutputState::Unknown,
    };
    let raw = profile::open_file(&dir.join("raw.ndjson.zst"))?;
    let recovered =
        recover(raw, |row| replay.row(row)).map_err(|e| format!("Ошибка чтения raw: {e}"))?;
    replay.flush();
    let (raw_tail, tail_diagnostic) = match recovered.tail {
        Tail::FrameBoundary => ("frame_boundary", None),
        Tail::IncompleteFrame => (
            "incomplete_frame",
            Some((Status::IncompleteFrame, "Оборван последний кадр")),
        ),
        Tail::CorruptFrame => (
            "corrupt_frame",
            Some((
                Status::CorruptFrame,
                "Первый повреждённый кадр и его хвост исключены",
            )),
        ),
    };
    if replay.first.is_none() {
        let next = recovered.records.saturating_add(1);
        if let Some((status, message)) = tail_diagnostic {
            replay.first = Some(Diagnostic::new(status, next, None, message));
        } else if replay.replayed == 0 || replay.verified_output != OutputState::OffVerified {
            replay.first = Some(Diagnostic::new(
                Status::Insufficient,
                next,
                None,
                "Сохранённого префикса недостаточно для полного результата",
            ));
        }
    }
    let status = replay.first.as_ref().map_or(Status::Matched, |d| d.status);
    let code = if matches!(status, Status::Matched) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    serde_json::to_string(&Report {
        run: replay.profile.meta.run,
        generation: replay.profile.meta.generation,
        status,
        raw_verified_records: recovered.records,
        raw_verified_frames: recovered.frames,
        raw_tail,
        replayed_ticks: replay.replayed,
        last_replayed_tick: replay.last_tick,
        historical_completion: "unknown",
        output_state_at_boundary: output_name(replay.verified_output),
        first_diagnostic: replay.first,
    })
    .map(|out| (out, code))
    .map_err(|e| format!("Ошибка формирования отчёта: {e}"))
}

struct Replay {
    profile: profile::Profile,
    operation: Option<Operation>,
    first: Option<Diagnostic>,
    replayed: u64,
    last_tick: Option<u64>,
    last_id: u64,
    verified_output: OutputState,
}
impl Replay {
    fn row(&mut self, row: Record<'_>) {
        if self.first.is_some() {
            return;
        }
        if row.run != self.profile.meta.run || row.generation != self.profile.meta.generation {
            self.first = Some(Diagnostic::new(
                Status::Mismatch,
                row.seq,
                Some(row.correlation),
                "Raw принадлежит другому прогону или поколению",
            ));
            return;
        }
        if self
            .operation
            .as_ref()
            .is_some_and(|op| op.id != row.correlation)
        {
            self.flush();
        }
        if self.first.is_some() {
            return;
        }
        let id = row.correlation;
        let terminal_run_record =
            id == Correlation::Run && matches!(row.event, Event::OutputEvidence { .. });
        let op = self
            .operation
            .get_or_insert_with(|| Operation::new(id, row.seq));
        if let Err(diagnostic) = op.push(row.seq, row.event) {
            self.first = Some(diagnostic);
        } else if terminal_run_record {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.first.is_some() {
            return;
        }
        if let Some(op) = self.operation.take() {
            match self.apply(&op) {
                Ok(()) => self.verified_output = self.profile.supervisor.output_state(),
                Err(diagnostic) => self.first = Some(diagnostic),
            }
        }
    }

    fn apply(&mut self, op: &Operation) -> Result<(), Diagnostic> {
        match op.id {
            Correlation::Run => {}
            Correlation::Request(id) | Correlation::Tick(id) => {
                if id <= self.last_id {
                    return Err(op.mismatch(op.first, "Нарушен порядок идентификаторов операций"));
                }
                self.last_id = id;
            }
        }
        let mut journal = CompareJournal::new(op);
        let effect = if let Some(action) = op.operator {
            let input = match action.0 {
                OperatorAction::Stop => stop_input(action.1),
                _ => op
                    .input
                    .as_ref()
                    .ok_or_else(|| op.insufficient("Отсутствуют входы операторской команды"))?
                    .monitor
                    .clone(),
            };
            let decision = self
                .profile
                .supervisor
                .operate(action.0, &input, &mut journal)
                .map_err(|_| {
                    op.mismatch(
                        op.first,
                        "Операторское действие невозможно в пересчитанном состоянии",
                    )
                })?;
            journal.finish()?;
            if action.0 == OperatorAction::Reset {
                self.profile.controller.reset_fault();
            }
            decision.effect
        } else if op.id == Correlation::Run {
            if op.wire.identity.is_none() {
                return Err(
                    op.insufficient("В Run отсутствует свидетельство восстановления или Stop")
                );
            }
            journal.finish()?;
            self.profile.supervisor.disconnect();
            self.profile
                .supervisor
                .connect()
                .map_err(|_| op.mismatch(op.first, "Невозможно восстановить сеанс M5"))?;
            self.profile.controller.reset_fault();
            Some(
                self.profile
                    .supervisor
                    .request_off()
                    .map_err(|_| op.mismatch(op.first, "Нет запроса начального OFF"))?,
            )
        } else if matches!(op.id, Correlation::Tick(_)) {
            let input = op
                .input
                .as_ref()
                .ok_or_else(|| op.insufficient("Отсутствуют входы такта"))?;
            let safety = self
                .profile
                .supervisor
                .observe(&input.monitor, &mut journal);
            let (effect, drive) = if self.profile.supervisor.state() == MachineState::Running {
                let control = self.profile.supervisor.regulate(
                    input.monitor.monotonic,
                    &mut self.profile.controller,
                    input.tick,
                    &mut journal,
                );
                (control.effect, control.currents)
            } else {
                (safety.effect, None)
            };
            journal.finish()?;
            if let Some(currents) = drive {
                op.check_drive(currents)?;
            } else if effect.is_none() {
                op.exact_intents(0)?;
                op.exact_writes(0)?;
            }
            effect
        } else {
            return Err(op.insufficient("В журнале нет операторского действия"));
        };
        if let Some(effect) = effect {
            self.apply_effect(op, effect)?;
        } else if !matches!(op.id, Correlation::Tick(_)) {
            op.exact_intents(0)?;
            op.exact_writes(0)?;
        }
        op.check_output(&self.profile.supervisor, effect.is_some())?;
        if let Correlation::Tick(id) = op.id {
            self.replayed += 1;
            self.last_tick = Some(id);
        }
        Ok(())
    }

    fn apply_effect(&mut self, op: &Operation, effect: Effect) -> Result<(), Diagnostic> {
        let supervisor = &mut self.profile.supervisor;
        match effect {
            Effect::DisableBoth(token) => {
                let recovering = op.id == Correlation::Run && op.operator.is_none();
                let intent = if recovering {
                    if !op.intents.is_empty()
                        || op.wire.writes.iter().any(|row| {
                            !matches!(
                                row.row.command,
                                Command::Off | Command::Drive { current_a: 0.0 }
                            )
                        })
                    {
                        return Err(
                            op.mismatch(op.first, "Недопустимая команда при восстановлении")
                        );
                    }
                    [op.wire.identity.expect("checked recovery identity"); 2]
                } else {
                    let intent = op.intent_pair(0, [Command::Off; 2])?;
                    op.exact_intents(2)?;
                    intent
                };
                let proof = op.proof_pair([Command::Off; 2], intent)?;
                if proof
                    .iter()
                    .any(|item| item.read <= proof[0].sent.max(proof[1].sent))
                {
                    return Err(op.mismatch(
                        op.first,
                        "Подтверждение OFF предшествует отправке обеих ветвей",
                    ));
                }
                for branch in Branch::ALL {
                    supervisor
                        .confirm_off(token, branch, OffEvidence::ReportedOff)
                        .map_err(|_| {
                            op.mismatch(op.first, "Подтверждение OFF не относится к запросу M5")
                        })?;
                }
                if recovering {
                    let last_off = proof[0].read.max(proof[1].read);
                    let zero = op.proof_pair([Command::ZeroPreset; 2], [last_off; 2])?;
                    op.serial(zero)?;
                    op.exact_writes(4)?;
                } else {
                    op.exact_writes(2)?;
                }
            }
            Effect::EnableBoth(token) => {
                let zero = op.intent_pair(0, [Command::ZeroPreset; 2])?;
                let on = op.intent_pair(2, [Command::On; 2])?;
                op.exact_intents(4)?;
                let zero_proof = op.proof_pair([Command::ZeroPreset; 2], zero)?;
                let on_proof = op.proof_pair([Command::On; 2], on)?;
                op.serial(zero_proof)?;
                if on
                    .iter()
                    .any(|seq| *seq <= zero_proof[0].read.max(zero_proof[1].read))
                    || on_proof[1].sent <= on_proof[0].read
                {
                    return Err(
                        op.mismatch(op.first, "Нарушен порядок нулевой уставки, ON и ответов")
                    );
                }
                op.exact_writes(4)?;
                confirm_on(supervisor, token, op)?;
                self.profile.controller.reset_fault();
            }
        }
        Ok(())
    }
}

fn confirm_on(s: &mut Supervisor, token: RequestToken, op: &Operation) -> Result<(), Diagnostic> {
    for branch in Branch::ALL {
        if !s.can_enable(token) {
            return Err(op.mismatch(op.first, "M5 не допускает подтверждённый в журнале ON"));
        }
        s.confirm_on(token, branch)
            .map_err(|_| op.mismatch(op.first, "Подтверждение ON не относится к запросу M5"))?;
    }
    Ok(())
}

struct Operation {
    id: Correlation,
    first: u64,
    rows: usize,
    input: Option<Frame>,
    operator: Option<(OperatorAction, Duration)>,
    expected: Vec<(u64, Vec<u8>)>,
    intents: Vec<CommandRow>,
    output: Option<OutputRow>,
    wire: Wire,
}

#[derive(Clone, Copy)]
struct CommandRow {
    seq: u64,
    channel: Channel,
    command: Command,
}
struct OutputRow {
    seq: u64,
    state: String,
    fault: Option<String>,
    fallback: bool,
}

impl Operation {
    fn new(id: Correlation, first: u64) -> Self {
        Self {
            id,
            first,
            rows: 0,
            input: None,
            operator: None,
            expected: Vec::new(),
            intents: Vec::new(),
            output: None,
            wire: Wire::default(),
        }
    }
    fn mismatch(&self, seq: u64, message: &'static str) -> Diagnostic {
        Diagnostic::new(Status::Mismatch, seq, Some(self.id), message)
    }
    fn insufficient(&self, message: &'static str) -> Diagnostic {
        Diagnostic::new(Status::Insufficient, self.first, Some(self.id), message)
    }

    fn push(&mut self, seq: u64, event: Event<'_>) -> Result<(), Diagnostic> {
        self.rows += 1;
        if self.rows > MAX_OPERATION_ROWS {
            return Err(self.insufficient("Превышен предел записей одной операции"));
        }
        if self.output.is_some() {
            return Err(self.mismatch(seq, "Запись после завершения той же операции"));
        }
        match &event {
            Event::Inputs { frame } => {
                if self.id == Correlation::Run
                    || self.input.is_some()
                    || !self.expected.is_empty()
                    || !self.intents.is_empty()
                {
                    return Err(self.mismatch(seq, "Повторные или запоздалые входы"));
                }
                self.input =
                    Some(Frame::from_raw(frame).map_err(|_| {
                        self.insufficient("Неподдерживаемые или неполные входы v1")
                    })?);
            }
            Event::Observation { .. } | Event::Control { .. } | Event::Operator { .. } => {
                if let Event::Operator { action, at_ns } = event {
                    let action = match action {
                        "arm" => OperatorAction::Arm,
                        "start" => OperatorAction::Start,
                        "reset" => OperatorAction::Reset,
                        "stop" => OperatorAction::Stop,
                        _ => return Err(self.insufficient("Неизвестное операторское действие")),
                    };
                    if self
                        .operator
                        .replace((action, Duration::from_nanos(at_ns)))
                        .is_some()
                        || (action != OperatorAction::Stop
                            && !matches!(self.id, Correlation::Request(_)))
                        || matches!(self.id, Correlation::Tick(_))
                    {
                        return Err(self.mismatch(
                            seq,
                            "Повторное или неверно связанное операторское действие",
                        ));
                    }
                }
                if self.id != Correlation::Run
                    && self.input.is_none()
                    && self.operator.is_none_or(|x| x.0 != OperatorAction::Stop)
                {
                    return Err(self.mismatch(seq, "Решение до первичных входов"));
                }
                if !self.intents.is_empty() {
                    return Err(self.mismatch(seq, "Решение записано после intent"));
                }
                self.expected.push((seq, encode_event(&event)));
            }
            Event::CommandIntent {
                channel, command, ..
            } => {
                self.intents.push(CommandRow {
                    seq,
                    channel: *channel,
                    command: *command,
                });
            }
            Event::OutputEvidence {
                state,
                fault,
                fallback_requested,
                ..
            } => {
                self.output = Some(OutputRow {
                    seq,
                    state: (*state).into(),
                    fault: fault.map(str::to_owned),
                    fallback: *fallback_requested,
                });
            }
            Event::Parameters { .. } => {
                return Err(self.insufficient("Профиль v1 не содержит полной истории параметров"))
            }
            Event::Dispatch { .. } | Event::Response { .. } => self.wire.add(seq, &event),
        }
        Ok(())
    }

    fn intent_pair(&self, offset: usize, commands: [Command; 2]) -> Result<[u64; 2], Diagnostic> {
        let mut seq = [0; 2];
        for index in 0..2 {
            let row = self
                .intents
                .get(offset + index)
                .ok_or_else(|| self.insufficient("Отсутствует intent одной из ветвей"))?;
            if row.channel != CHANNELS[index] || !same_command(row.command, commands[index]) {
                return Err(self.mismatch(
                    row.seq,
                    "Типизированная команда отличается от пересчитанной",
                ));
            }
            seq[index] = row.seq;
        }
        Ok(seq)
    }

    fn exact_intents(&self, count: usize) -> Result<(), Diagnostic> {
        if let Some(extra) = self.intents.get(count) {
            return Err(self.mismatch(extra.seq, "Лишнее намерение команды"));
        }
        Ok(())
    }

    fn exact_writes(&self, count: usize) -> Result<(), Diagnostic> {
        if let Some(extra) = self.wire.writes.get(count) {
            return Err(if extra.sent {
                self.mismatch(extra.row.seq, "Лишняя отправка команды в эмулятор")
            } else {
                self.insufficient("Неизвестен исход дополнительной попытки отправки команды")
            });
        }
        Ok(())
    }

    fn proof_pair(
        &self,
        commands: [Command; 2],
        after: [u64; 2],
    ) -> Result<[Proof; 2], Diagnostic> {
        let after = after[0].max(after[1]);
        let proof = |i: usize| {
            self.wire
                .proof(CHANNELS[i], commands[i], after)
                .ok_or_else(|| {
                    self.insufficient(
                        "Отсутствует связанное свидетельство отправки и ответа одной из ветвей",
                    )
                })
        };
        Ok([proof(0)?, proof(1)?])
    }

    fn check_drive(&self, currents: [Amperes; 2]) -> Result<(), Diagnostic> {
        let commands = currents.map(|a| Command::Drive { current_a: a.get() });
        let intent = self.intent_pair(0, commands)?;
        self.exact_intents(2)?;
        self.serial(self.proof_pair(commands, intent)?)?;
        self.exact_writes(2)
    }

    fn serial(&self, mut proof: [Proof; 2]) -> Result<(), Diagnostic> {
        proof.sort_unstable_by_key(|branch| branch.sent);
        if proof[1].sent <= proof[0].read {
            return Err(self.mismatch(proof[1].sent, "Транзакции ветвей перекрываются"));
        }
        Ok(())
    }

    fn check_output(&self, supervisor: &Supervisor, expected: bool) -> Result<(), Diagnostic> {
        if let Some(row) = &self.output {
            if !expected
                || row.state != output_name(supervisor.output_state())
                || row.fault.as_deref()
                    != supervisor
                        .fault_reason()
                        .map(|reason| fault_issue(reason).code)
                || row.fallback != supervisor.fallback_requested()
            {
                return Err(self.mismatch(
                    row.seq,
                    "Сохранённое состояние выхода отличается от восстановленного",
                ));
            }
        } else if expected {
            return Err(self.insufficient("Отсутствует итоговое свидетельство выхода"));
        }
        Ok(())
    }
}

struct Frame {
    monitor: MonitorInput,
    tick: Tick,
}
impl Frame {
    fn from_raw(frame: &Inputs<'_>) -> Result<Self, String> {
        if frame.power.map(|row| row.channel) != CHANNELS
            || frame.temperatures.map(|row| row.point)
                != [
                    ThermalPoint::Stage1,
                    ThermalPoint::Stage2,
                    ThermalPoint::Tf,
                    ThermalPoint::Jacket,
                ]
        {
            return Err("Неверный порядок каналов v1".into());
        }
        let now = Duration::from_nanos(frame.measured_ns);
        let phase = match frame.cooling_phase {
            "cooling" => CoolingPhase::Cooling,
            "stable" => CoolingPhase::Stable,
            "warming" => CoolingPhase::Warming,
            _ => return Err("Неизвестная фаза".into()),
        };
        let demand = match frame.demand {
            raw::Demand::Power { requested_w } => {
                Demand::Power(quantity(Watts::new(requested_w), "requested_w")?)
            }
            _ => return Err("Входной режим вне профиля v1".into()),
        };
        let (a, sample_a) = iv(frame.power[0])?;
        let (b, sample_b) = iv(frame.power[1])?;
        let temperatures = match (
            temperature(frame.temperatures[0])?,
            temperature(frame.temperatures[1])?,
        ) {
            (Some((stage1_k, stage1_channel, a)), Some((stage2_k, stage2_channel, b))) => {
                Some(Temperatures {
                    stage1_k,
                    stage2_k,
                    measured_at: a.min(b),
                    stage1_channel,
                    stage2_channel,
                })
            }
            _ => None,
        };
        let samples = [sample_a, sample_b];
        Ok(Self {
            monitor: MonitorInput {
                monotonic: now,
                wall_time_s: quantity(Seconds::new(frame.wall_time_s), "wall_time_s")?,
                branches: [a, b],
                branch_channels: Branch::ALL,
                temperatures,
                allow_stale_once: frame.allow_stale_once,
                source_connected: frame.source_connected,
                thermometer_connected: frame.thermometer_connected,
                missed_tick: frame.missed_tick,
                expect_tick: frame.expect_tick,
                queue_fault: frame.queue_fault,
                model_valid: frame.model_valid,
                journal_ok: frame.journal_ok,
                cooling_phase: phase,
            },
            tick: Tick {
                demand,
                permit: HeatPermit::Inhibit,
                branches_available: samples.map(|x| x.is_some()),
                measurements: samples,
                now: seconds(now),
                dt: seconds(Duration::from_nanos(frame.dt_ns)),
            },
        })
    }
}

fn iv(row: raw::PowerSample) -> Result<(Option<BranchReading>, Option<TimedPowerSample>), String> {
    if row.quality != Quality::Measured {
        return Ok((None, None));
    }
    let a = quantity(row.current_a.and_then(Amperes::new), "current_a")?;
    let v = quantity(row.voltage_v.and_then(Volts::new), "voltage_v")?;
    let target = quantity(row.target_w.and_then(Watts::new), "target_w")?;
    let at = row.read_started_ns.ok_or("Нет времени чтения I/V")?;
    Ok((
        Some(BranchReading {
            current_a: a,
            voltage_v: v,
            command_power_w: target,
        }),
        Some(TimedPowerSample {
            at: seconds(Duration::from_nanos(at)),
            sample: PowerSample {
                voltage: v,
                current: a,
            },
        }),
    ))
}

fn temperature(row: raw::TemperatureSample) -> Result<Option<(Kelvin, u8, Duration)>, String> {
    if row.quality != Quality::Measured {
        return Ok(None);
    }
    let k = quantity(row.temperature_k.and_then(Kelvin::new), "temperature_k")?;
    let channel = row.channel.ok_or("Нет канала термометра")?;
    let at = row.read_started_ns.ok_or("Нет времени чтения термометра")?;
    Ok(Some((k, channel, Duration::from_nanos(at))))
}

fn seconds(time: Duration) -> Seconds {
    Seconds::new(time.as_secs_f64()).expect("finite u64 timestamp")
}

fn stop_input(at: Duration) -> MonitorInput {
    // The Stop path in Supervisor only reads monotonic. The record contains
    // no measurement, and none is invented for this operation.
    MonitorInput {
        monotonic: at,
        wall_time_s: seconds(Duration::ZERO),
        branches: [None; 2],
        branch_channels: Branch::ALL,
        temperatures: None,
        allow_stale_once: false,
        source_connected: false,
        thermometer_connected: false,
        missed_tick: false,
        expect_tick: false,
        queue_fault: false,
        model_valid: false,
        journal_ok: false,
        cooling_phase: CoolingPhase::Stable,
    }
}

fn output_name(output: OutputState) -> &'static str {
    match output {
        OutputState::Unknown => "unknown",
        OutputState::OffVerified => "off_verified",
        OutputState::OnVerified => "on_verified",
    }
}

fn state_name(state: MachineState) -> &'static str {
    match state {
        MachineState::Disconnected => "disconnected",
        MachineState::Observing => "observing",
        MachineState::OutputOffVerified => "output_off_verified",
        MachineState::Armed => "armed",
        MachineState::Running => "running",
        MachineState::Stopping => "stopping",
        MachineState::FaultLatched => "fault_latched",
    }
}

fn control_fault(fault: ControlFault) -> &'static str {
    match fault {
        ControlFault::BranchLost => "branch_lost",
        ControlFault::MeasurementUnavailable => "measurement_unavailable",
        ControlFault::MeasurementStale => "measurement_stale",
        ControlFault::TemperatureInterlock => "temperature_interlock",
        ControlFault::Clock => "clock",
        ControlFault::Calculation => "calculation",
    }
}

fn encode_event(event: &Event<'_>) -> Vec<u8> {
    serde_json::to_vec(event).expect("typed, finite replay event")
}

struct CompareJournal<'a> {
    op: &'a Operation,
    next: usize,
    issue: Option<Diagnostic>,
}
impl<'a> CompareJournal<'a> {
    fn new(op: &'a Operation) -> Self {
        Self {
            op,
            next: 0,
            issue: None,
        }
    }
    fn finish(self) -> Result<(), Diagnostic> {
        if let Some(issue) = self.issue {
            return Err(issue);
        }
        if let Some((seq, _)) = self.op.expected.get(self.next) {
            return Err(self.op.mismatch(*seq, "Лишнее сохранённое решение"));
        }
        Ok(())
    }
}
impl SafetyJournal for CompareJournal<'_> {
    fn record(&mut self, row: SafetyRecord<'_>) -> Result<(), JournalError> {
        if self.issue.is_none() {
            let event = match row {
                SafetyRecord::Observation {
                    at,
                    state,
                    faults,
                    warnings,
                } => Event::Observation {
                    at_ns: at.as_nanos() as u64,
                    state: state_name(state),
                    faults: faults.iter().copied().map(fault_issue).collect(),
                    warnings: warnings
                        .iter()
                        .map(|warning| match warning {
                            Warning::HeadApproaching => "head_approaching",
                            Warning::MeasurementStale => "measurement_stale",
                        })
                        .collect(),
                },
                SafetyRecord::Operator { at, action } => Event::Operator {
                    at_ns: at.as_nanos() as u64,
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
                    at_ns: at.as_nanos() as u64,
                    requested_w: output.requested.map(Watts::get),
                    limited_w: Some(output.limited.get()),
                    measured_w: output.measured.map(Watts::get),
                    targets_w: Some(output.targets.map(Watts::get)),
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
                    fault: fault.map(control_fault),
                },
            };
            self.issue = match self.op.expected.get(self.next) {
                None => Some(self.op.insufficient("Не записано пересчитанное решение")),
                Some((seq, recorded)) if *recorded != encode_event(&event) => Some(
                    self.op
                        .mismatch(*seq, "Пересчитанное решение M4/M5 отличается побитово"),
                ),
                _ => None,
            };
        }
        self.next += 1;
        // A comparison result is not an input fault in the original loop.
        Ok(())
    }
}

fn same_command(left: Command, right: Command) -> bool {
    match (left, right) {
        (Command::Drive { current_a: a }, Command::Drive { current_a: b }) => {
            a.to_bits() == b.to_bits()
        }
        _ => left == right,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Query {
    Identity,
    Output(Channel),
    Level(Channel),
}
impl Query {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "*IDN?" => Self::Identity,
            "print(smua.source.output)" => Self::Output(Channel::A),
            "print(smub.source.output)" => Self::Output(Channel::B),
            "print(smua.source.leveli)" => Self::Level(Channel::A),
            "print(smub.source.leveli)" => Self::Level(Channel::B),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
struct Proof {
    sent: u64,
    read: u64,
}

struct WireWrite {
    row: CommandRow,
    sent: bool,
}

struct Readback {
    query: Query,
    sent_at: u64,
    result: Option<(u64, f64)>,
}

#[derive(Default)]
struct Wire {
    identity: Option<u64>,
    pending: Option<usize>,
    writes: Vec<WireWrite>,
    replies: Vec<Readback>,
}
impl Wire {
    fn add(&mut self, seq: u64, event: &Event<'_>) {
        match event {
            Event::Dispatch {
                source: "keithley",
                command,
                status,
                ..
            } => {
                self.pending = None;
                let sent = *status == "sent";
                if let Some(query) = Query::parse(command) {
                    if sent {
                        self.pending = Some(self.replies.len());
                    }
                    self.replies.push(Readback {
                        query,
                        sent_at: seq,
                        result: None,
                    });
                } else if let Some((channel, command)) = wire_write(command) {
                    self.writes.push(WireWrite {
                        row: CommandRow {
                            seq,
                            channel,
                            command,
                        },
                        sent,
                    });
                }
            }
            Event::Response {
                source: "keithley",
                command,
                bytes,
                status,
                ..
            } => {
                let Some(index) = self.pending.take() else {
                    return;
                };
                let reply = &mut self.replies[index];
                if Query::parse(command) != Some(reply.query)
                    || *status != "received"
                    || seq <= reply.sent_at
                {
                    return;
                }
                let Some(bytes) = bytes.as_deref() else {
                    return;
                };
                let Ok(text) = std::str::from_utf8(bytes) else {
                    return;
                };
                if reply.query == Query::Identity {
                    if text
                        .split(',')
                        .nth(1)
                        .is_some_and(|model| model.trim() == "MODEL 2604B")
                    {
                        self.identity = Some(seq);
                    }
                } else if let Ok(value) = text.trim().parse::<f64>() {
                    if value.is_finite() {
                        reply.result = Some((seq, value));
                    }
                }
            }
            _ => {}
        }
    }

    fn proof(&self, channel: Channel, command: Command, after: u64) -> Option<Proof> {
        let (query, expected, write) = match command {
            Command::On => (Query::Output(channel), 1.0, command),
            Command::Off => (Query::Output(channel), 0.0, command),
            Command::ZeroPreset => (
                Query::Level(channel),
                0.0,
                Command::Drive { current_a: 0.0 },
            ),
            Command::Drive { current_a } => (Query::Level(channel), current_a, command),
        };
        let issued = self.writes.iter().find(|row| {
            row.row.seq > after
                && row.row.channel == channel
                && same_command(row.row.command, write)
        })?;
        if !issued.sent {
            return None;
        }
        let superseded = self
            .writes
            .iter()
            .find(|row| row.row.seq > issued.row.seq && row.row.channel == channel);
        let (read, value) = self
            .replies
            .iter()
            .find(|reply| reply.query == query && reply.sent_at > issued.row.seq)?
            .result?;
        // The simulator's decimal readback uses the existing driver's tolerance.
        if superseded.is_some_and(|row| read >= row.row.seq)
            || (value - expected).abs() > 1e-9 * expected.abs()
        {
            return None;
        }
        Some(Proof {
            sent: issued.row.seq,
            read,
        })
    }
}

fn wire_write(text: &str) -> Option<(Channel, Command)> {
    let (branch, body) = text.split_once('.')?;
    let channel = match branch {
        "smua" => Channel::A,
        "smub" => Channel::B,
        _ => return None,
    };
    let command = if let Some(level) = body.strip_prefix("source.leveli = ") {
        let current_a = level.parse::<f64>().ok().filter(|a| a.is_finite())?;
        Command::Drive { current_a }
    } else {
        let value = body
            .strip_prefix("source.output = ")?
            .strip_prefix(branch)?
            .strip_prefix('.')?;
        match value {
            "OUTPUT_ON" => Command::On,
            "OUTPUT_OFF" => Command::Off,
            _ => return None,
        }
    };
    Some((channel, command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn dispatch(command: &str) -> Event<'_> {
        Event::Dispatch {
            source: "keithley",
            command: Cow::Borrowed(command),
            status: "sent",
            at_ns: 0,
        }
    }
    fn response(command: &str) -> Event<'_> {
        Event::Response {
            source: "keithley",
            command: Cow::Borrowed(command),
            bytes: Some(Cow::Borrowed(b"0")),
            status: "received",
            at_ns: 0,
        }
    }

    #[test]
    fn readback_requires_a_causal_send_and_query() {
        let mut wire = Wire::default();
        let query = "print(smua.source.output)";
        wire.add(1, &response(query));
        wire.add(2, &dispatch("smua.source.output = smua.OUTPUT_OFF"));
        wire.add(3, &response(query));
        assert!(wire.proof(Channel::A, Command::Off, 0).is_none());
        wire.add(4, &dispatch(query));
        wire.add(5, &response(query));
        assert_eq!(wire.proof(Channel::A, Command::Off, 0).unwrap().read, 5);
        assert!(wire.proof(Channel::B, Command::Off, 0).is_none());
    }

    #[test]
    fn stale_response_cannot_prove_a_new_off() {
        let mut wire = Wire::default();
        let query = "print(smua.source.output)";
        wire.add(1, &dispatch("smua.source.output = smua.OUTPUT_OFF"));
        wire.add(2, &dispatch(query));
        wire.add(3, &response(query));
        wire.add(4, &dispatch("smua.source.output = smua.OUTPUT_OFF"));
        assert!(wire.proof(Channel::A, Command::Off, 3).is_none());
    }
}
