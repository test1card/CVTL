// FILE: crates/cvtl-daemon/src/operator.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Own the simulated operator run and project accepted observations
//   SCOPE: Run commands, priority stop, M5 evidence, and bounded live history
//   DEPENDS: M-CVTL-SIM, M-CVTL-SAFETY, M-CVTL-STORE, M-CVTL-CLI
//   LINKS: M-CVTL-DAEMON
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Operator - command admission and immutable snapshots
//   State - operator results and live run state
// END_MODULE_MAP

use std::{
    cell::RefCell,
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_cli::{TimelineSample, TIMELINE_LIMIT};
use cvtl_core::units::{Kilograms, WattsPerKelvin};
use cvtl_physics::scenario::{BodySetup, BridgeSetup};
use cvtl_safety::{
    machine::{MachineState, OutputState},
    supervisor::OperatorAction,
};
use cvtl_sim::{
    keithley::KeithleySimulator,
    lakeshore::LakeShoreSim,
    modes::{BodyEdit, ModeSetup},
    recording::{test_only_control_config, RecordingLoop, PERIOD},
};
use cvtl_store::{
    record::{
        A1ChangeV1, Correlation, Event, Inputs, ModeBodyChangeV1, ModeChangeOutcomeV1,
        ModeCompensationIssue, ModeCompensationV1, Quality,
    },
    session::{fault_issue, CloseResult, Receipt},
};
use serde::{Deserialize, Serialize};

use crate::{
    archive,
    scenario::{Estimate, Setup},
};

const REQUEST_LIFETIME: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_millis(5);
const EVENT_LIMIT: usize = 64;

pub(crate) struct Prepared {
    pub setup: Setup,
    pub estimate: Estimate,
}

#[derive(Deserialize)]
pub(crate) struct ActionRequest {
    run: String,
    #[serde(flatten)]
    action: Action,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum Action {
    Arm,
    Start { confirmed: bool },
    Recover,
    Reset { confirmed: bool },
    Stop,
    Fault,
    Change(BodyEdit),
}

enum Command {
    Create(Arc<Prepared>),
    Act(ActionRequest),
}

struct Pending {
    id: u64,
    at: Instant,
    command: Command,
    cancelled_id: Option<u64>,
}

impl Pending {
    fn is_stop(&self) -> bool {
        matches!(
            self.command,
            Command::Act(ActionRequest {
                action: Action::Stop,
                ..
            })
        )
    }
}

#[derive(Default)]
struct Mailbox {
    active_run: Option<String>,
    closing: bool,
    ordinary: Option<Pending>,
    stop: Option<Pending>,
}

impl Mailbox {
    fn validate(&self, command: &Command) -> Result<(), &'static str> {
        match command {
            Command::Create(_) if self.active_run.is_some() => {
                Err("Сначала завершите текущий прогон")
            }
            Command::Act(request) if self.active_run.as_deref() != Some(request.run.as_str()) => {
                Err("Команда относится к другому прогону")
            }
            Command::Act(_) if self.closing => Err("Прогон уже закрывается"),
            Command::Act(ActionRequest {
                action: Action::Start { confirmed: false } | Action::Reset { confirmed: false },
                ..
            }) => Err("Действие требует подтверждения"),
            _ => Ok(()),
        }
    }

    fn enqueue(&mut self, id: u64, command: Command, at: Instant) -> Result<(), &'static str> {
        self.validate(&command)?;
        if self.stop.is_some() {
            return Err("Остановка уже ожидается");
        }
        let mut pending = Pending {
            id,
            at,
            command,
            cancelled_id: None,
        };
        if let Command::Act(ActionRequest {
            run,
            action: Action::Stop,
        }) = &pending.command
        {
            if self.ordinary.as_ref().is_some_and(
                |queued| matches!(&queued.command, Command::Act(request) if request.run == *run),
            ) {
                pending.cancelled_id = self.ordinary.take().map(|queued| queued.id);
            }
            self.stop = Some(pending);
        } else if self.ordinary.is_some() {
            return Err("Другая команда уже ожидает исполнения");
        } else {
            self.ordinary = Some(pending);
        }
        Ok(())
    }

    fn take(
        &mut self,
        now: Instant,
        allow_ordinary: bool,
    ) -> Option<(Pending, Result<(), &'static str>)> {
        let pending = self
            .stop
            .take()
            .or_else(|| allow_ordinary.then(|| self.ordinary.take()).flatten())?;
        let mut valid = self.validate(&pending.command);
        if valid.is_ok() && now.saturating_duration_since(pending.at) > REQUEST_LIFETIME {
            valid = Err("Истёк срок действия команды");
        }
        if valid.is_ok() && pending.is_stop() {
            self.closing = true;
        }
        Some((pending, valid))
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct ActionResult {
    id: u64,
    ok: bool,
    message: Option<String>,
    cancelled_id: Option<u64>,
}

impl ActionResult {
    fn new(id: u64, result: Result<(), String>, cancelled_id: Option<u64>) -> Self {
        Self {
            id,
            ok: result.is_ok(),
            message: result.err(),
            cancelled_id,
        }
    }
}

#[derive(Clone, Serialize)]
struct RunEvent {
    seq: u64,
    kind: &'static str,
    at_ns: u64,
    code: String,
}

#[derive(Clone, Copy, Serialize)]
struct AppliedChange {
    tick: u64,
    seq: u64,
    change: A1ChangeV1,
}

#[derive(Clone, Serialize)]
struct ModeChange {
    tick: u64,
    seq: u64,
    change: ModeBodyChangeV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct EffectiveParameters {
    mass_kg: f64,
    scale: f64,
    g_w_per_k: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    material: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bridge: Option<BridgeSetup>,
}

impl EffectiveParameters {
    fn new(body: &BodySetup, explicit: bool) -> Self {
        Self {
            mass_kg: body.mass_kg,
            scale: body.scale,
            g_w_per_k: constant_conductance(&body.bridge),
            material: explicit.then(|| body.material.clone()),
            bridge: explicit.then(|| body.bridge.clone()),
        }
    }

    fn apply(&mut self, edit: BodyEdit) {
        self.mass_kg = edit.mass_kg.unwrap_or(self.mass_kg);
        self.scale = edit.scale.unwrap_or(self.scale);
        if let Some(material) = edit.material {
            self.material = Some(material);
        }
        let bridge = edit.bridge.or_else(|| {
            edit.g_w_per_k
                .map(|conductance_w_per_k| BridgeSetup::Constant {
                    conductance_w_per_k,
                })
        });
        if let Some(bridge) = bridge {
            self.g_w_per_k = constant_conductance(&bridge);
            self.bridge = Some(bridge);
        }
    }
}

fn constant_conductance(bridge: &BridgeSetup) -> Option<f64> {
    match *bridge {
        BridgeSetup::Constant {
            conductance_w_per_k,
        } => Some(conductance_w_per_k),
        _ => None,
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct RunView {
    id: String,
    state: &'static str,
    output_state: &'static str,
    fault: Option<&'static str>,
    fallback_requested: bool,
    ready: bool,
    writer_ready: bool,
    source_ready: bool,
    thermometer_ready: bool,
    observation_clear: bool,
    journal: &'static str,
    archive_error: Option<String>,
    resistance_ohms: [f64; 2],
    measured_resistance_ohms: [Option<f64>; 2],
    estimate: Estimate,
    parameters: Option<EffectiveParameters>,
    samples_seen: u64,
    samples: VecDeque<TimelineSample>,
    events: VecDeque<RunEvent>,
    last_change: Option<AppliedChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<ModeSetup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compensation_ready: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    off_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_mode_change: Option<ModeChange>,
}

#[derive(Serialize)]
pub(crate) struct State {
    test_faults: bool,
    run: Option<RunView>,
    last_action: Option<ActionResult>,
}

#[derive(Default)]
struct Shared {
    mailbox: Mailbox,
    run: Option<Arc<RunView>>,
    last_action: Option<ActionResult>,
    shutdown: bool,
}

pub(crate) struct Operator {
    shared: Arc<Mutex<Shared>>,
    owner: Option<JoinHandle<()>>,
    test_faults: bool,
}

impl Operator {
    pub(crate) fn launch(runs: PathBuf, test_faults: bool) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let actor = Arc::clone(&shared);
        let owner = thread::spawn(move || owner_loop(&runs, &actor));
        Self {
            shared,
            owner: Some(owner),
            test_faults,
        }
    }

    pub(crate) fn snapshot(&self) -> State {
        let (run, last_action) = {
            let shared = self.shared.lock().unwrap();
            (shared.run.clone(), shared.last_action.clone())
        };
        State {
            test_faults: self.test_faults,
            run: run.as_deref().cloned(),
            last_action,
        }
    }

    pub(crate) fn create(&self, id: u64, prepared: Arc<Prepared>) -> Result<(), &'static str> {
        self.enqueue(id, Command::Create(prepared))
    }

    pub(crate) fn submit(&self, id: u64, request: ActionRequest) -> Result<(), &'static str> {
        if matches!(request.action, Action::Fault) && !self.test_faults {
            return Err("Имитация отказа выключена");
        }
        self.enqueue(id, Command::Act(request))
    }

    fn enqueue(&self, id: u64, command: Command) -> Result<(), &'static str> {
        self.shared
            .lock()
            .unwrap()
            .mailbox
            .enqueue(id, command, Instant::now())?;
        if let Some(owner) = &self.owner {
            owner.thread().unpark();
        }
        Ok(())
    }
}

impl Drop for Operator {
    fn drop(&mut self) {
        self.shared.lock().unwrap().shutdown = true;
        if let Some(owner) = self.owner.take() {
            owner.thread().unpark();
            let _ = owner.join();
        }
    }
}

struct Projection {
    parameters: Option<EffectiveParameters>,
    mode: Option<ModeSetup>,
    samples_seen: u64,
    samples: VecDeque<TimelineSample>,
    events: VecDeque<RunEvent>,
    last_change: Option<AppliedChange>,
    last_mode_change: Option<ModeChange>,
    off_reason: Option<String>,
    readings: Option<Readings>,
    observation_clear: bool,
}

struct Readings {
    correlation: Correlation,
    power_ns: Option<u64>,
    thermal_ns: Option<u64>,
    resistance: [Option<f64>; 2],
}

impl Projection {
    fn new(setup: &Setup) -> Self {
        Self {
            parameters: match setup.mode {
                None => Some(EffectiveParameters::new(&setup.body, false)),
                Some(ModeSetup::VirtualBody { .. }) => {
                    Some(EffectiveParameters::new(&setup.body, true))
                }
                _ => None,
            },
            mode: setup.mode.clone(),
            samples_seen: 0,
            samples: VecDeque::new(),
            events: VecDeque::new(),
            last_change: None,
            last_mode_change: None,
            off_reason: None,
            readings: None,
            observation_clear: false,
        }
    }

    fn accept(&mut self, receipt: Receipt, events: &[Event<'_>]) {
        for (offset, event) in events.iter().enumerate() {
            let seq = receipt.first_seq + offset as u64;
            match event {
                Event::Inputs { frame } => self.inputs(receipt.correlation, seq, frame),
                Event::Observation {
                    at_ns,
                    faults,
                    warnings,
                    ..
                } => {
                    self.observation_clear = faults.is_empty()
                        && self
                            .readings
                            .as_ref()
                            .is_some_and(|readings| readings.correlation == receipt.correlation);
                    if let Some(code) = faults
                        .first()
                        .map(|fault| fault.code)
                        .or_else(|| warnings.first().copied())
                    {
                        self.event(seq, "observation", *at_ns, code);
                    }
                }
                Event::Control {
                    at_ns,
                    warning,
                    fault,
                    ..
                } => {
                    if let Some(sample) = self.samples.back_mut() {
                        if receipt.correlation == Correlation::Tick(sample.tick) {
                            sample.apply_control(event);
                        }
                    }
                    if let Some(code) = fault.or(*warning) {
                        self.event(seq, "control", *at_ns, code);
                    }
                }
                Event::Operator { at_ns, action } => {
                    if self.mode.is_some() && *action == "stop" {
                        self.off_reason = Some("operator_stop".into());
                    }
                    self.event(seq, "operator", *at_ns, action);
                }
                Event::OutputEvidence {
                    at_ns,
                    state,
                    fault,
                    ..
                } => {
                    if self.mode.is_some() {
                        self.output(receipt.correlation, state, *fault);
                    }
                    self.event(seq, "output_evidence", *at_ns, state)
                }
                _ => {}
            }
        }
    }

    fn inputs(&mut self, correlation: Correlation, seq: u64, frame: &Inputs<'_>) {
        self.observation_clear = false;
        let power_ns = frame
            .source_connected
            .then(|| {
                oldest(frame.power.map(|sample| {
                    (sample.quality == Quality::Measured)
                        .then_some(sample.read_started_ns)
                        .flatten()
                }))
            })
            .flatten();
        let thermal_ns = (frame.thermometer_connected && frame.model_valid)
            .then(|| {
                oldest(frame.temperatures.map(|sample| {
                    (sample.quality == Quality::Measured)
                        .then_some(sample.read_started_ns)
                        .flatten()
                }))
            })
            .flatten();
        self.readings = Some(Readings {
            correlation,
            power_ns,
            thermal_ns,
            resistance: frame.power.map(|sample| {
                let current = sample.current_a?;
                let voltage = sample.voltage_v?;
                let value = voltage / current;
                (sample.quality == Quality::Measured && value > 0.0 && value.is_finite())
                    .then_some(value)
            }),
        });

        if let Correlation::Tick(tick) = correlation {
            let mut sample = TimelineSample::from_inputs(tick, seq, frame);
            if let Some(mode) = &self.mode {
                sample.apply_operator_mode(mode, frame.mode_step_v1.as_ref());
            }
            if let (Some(change), Some(parameters)) = (sample.change, self.parameters.as_mut()) {
                parameters.mass_kg = change.mass_kg.unwrap_or(parameters.mass_kg);
                parameters.scale = change.scale.unwrap_or(parameters.scale);
                parameters.g_w_per_k = change.bridge_w_per_k.or(parameters.g_w_per_k);
                self.last_change = Some(AppliedChange { tick, seq, change });
                self.event(seq, "parameters", frame.measured_ns, "applied");
            }
            if let Some(mode) = &sample.mode_v1 {
                if let Some(change) = &mode.change {
                    let reason = match change.outcome {
                        ModeChangeOutcomeV1::Applied { .. } => {
                            if let (Some(parameters), Ok(edit)) = (
                                self.parameters.as_mut(),
                                serde_json::from_value(change.request.clone()),
                            ) {
                                parameters.apply(edit);
                            }
                            None
                        }
                        ModeChangeOutcomeV1::Rejected => {
                            Some(compensation_issue(mode.compensation).unwrap_or(
                                if !frame.model_valid || sample.bridge_flow_w.is_some() {
                                    "body_edit_invalid_at_tick"
                                } else {
                                    "port_unavailable"
                                },
                            ))
                        }
                    };
                    self.last_mode_change = Some(ModeChange {
                        tick,
                        seq,
                        change: change.clone(),
                        reason,
                    });
                    self.event(
                        seq,
                        "parameters",
                        frame.measured_ns,
                        reason.unwrap_or("applied"),
                    );
                }
            }
            self.samples_seen = self.samples_seen.saturating_add(1);
            if self.samples.len() == TIMELINE_LIMIT {
                self.samples.pop_front();
            }
            self.samples.push_back(sample);
        }
    }

    fn output(&mut self, correlation: Correlation, state: &str, fault: Option<&str>) {
        if state == "on_verified" {
            self.off_reason = None;
        } else if let Some(fault) = fault {
            self.off_reason = Some(fault.to_owned());
        } else if let Some(sample) = self
            .samples
            .back()
            .filter(|sample| correlation == Correlation::Tick(sample.tick))
        {
            let reason =
                compensation_issue(sample.mode_v1.as_ref().and_then(|mode| mode.compensation))
                    .or_else(|| {
                        sample
                            .requested_w
                            .is_some_and(|power| power < 0.0)
                            .then_some("negative_request")
                    });
            self.off_reason = reason.map(str::to_owned);
        }
    }

    fn event(&mut self, seq: u64, kind: &'static str, at_ns: u64, code: &str) {
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
        }
        self.events.push_back(RunEvent {
            seq,
            kind,
            at_ns,
            code: code.into(),
        });
    }
}

fn compensation_issue(compensation: Option<ModeCompensationV1>) -> Option<&'static str> {
    let ModeCompensationV1::Unavailable { reason } = compensation? else {
        return None;
    };
    Some(match reason {
        ModeCompensationIssue::MissingTf => "compensation_missing_tf",
        ModeCompensationIssue::PairNotReady => "compensation_pair_not_ready",
        ModeCompensationIssue::FixtureInvalid => "compensation_fixture_invalid",
    })
}

fn oldest<const N: usize>(times: [Option<u64>; N]) -> Option<u64> {
    times
        .into_iter()
        .try_fold(u64::MAX, |oldest, at| at.map(|value| oldest.min(value)))
}

fn fresh(at: Option<u64>, elapsed: Duration, limit: Duration) -> bool {
    at.and_then(|ns| elapsed.checked_sub(Duration::from_nanos(ns)))
        .is_some_and(|age| age <= limit)
}

struct ActiveRun<'a> {
    id: String,
    prepared: Arc<Prepared>,
    source: &'a KeithleySimulator,
    recording: RecordingLoop<'a>,
    epoch: Instant,
    projection: Rc<RefCell<Projection>>,
    journal: &'static str,
    archive_error: Option<String>,
}

impl ActiveRun<'_> {
    fn preflight(&self, writer_ready: bool) -> (bool, bool, bool, bool) {
        let projection = self.projection.borrow();
        let elapsed = self.epoch.elapsed();
        let source = projection.readings.as_ref().is_some_and(|readings| {
            fresh(
                readings.power_ns,
                elapsed,
                Duration::from_secs_f64(test_only_control_config().max_sample_age.get()),
            )
        });
        let thermometer = projection.readings.as_ref().is_some_and(|readings| {
            fresh(
                readings.thermal_ns,
                elapsed,
                Duration::from_millis(self.prepared.estimate.limits.max_temperature_age_ms),
            )
        });
        let clear = projection.observation_clear;
        let ready = writer_ready
            && self.journal == "recording"
            && self.recording.state() == MachineState::OutputOffVerified
            && self.recording.output_state() == OutputState::OffVerified
            && source
            && thermometer
            && clear
            && self.recording.compensation_ready();
        (ready, source, thermometer, clear)
    }

    fn publish(&self, shared: &Mutex<Shared>, writer_ready: bool, action: Option<ActionResult>) {
        let (ready, source_ready, thermometer_ready, observation_clear) =
            self.preflight(writer_ready);
        let projection = self.projection.borrow();
        let view = Arc::new(RunView {
            id: self.id.clone(),
            state: machine_code(self.recording.state()),
            output_state: output_code(self.recording.output_state()),
            fault: self
                .recording
                .fault_reason()
                .map(|fault| fault_issue(fault).code),
            fallback_requested: self.recording.fallback_requested(),
            ready,
            writer_ready,
            source_ready,
            thermometer_ready,
            observation_clear,
            journal: self.journal,
            archive_error: self.archive_error.clone(),
            resistance_ohms: self.source.resistance_ohms().map(|value| value.get()),
            measured_resistance_ohms: if source_ready {
                projection
                    .readings
                    .as_ref()
                    .map_or([None; 2], |readings| readings.resistance)
            } else {
                [None; 2]
            },
            estimate: self.prepared.estimate.clone(),
            parameters: projection.parameters.clone(),
            samples_seen: projection.samples_seen,
            samples: projection.samples.clone(),
            events: projection.events.clone(),
            last_change: projection.last_change,
            mode: projection.mode.clone(),
            compensation_ready: matches!(
                projection.mode,
                Some(ModeSetup::VirtualBody {
                    compensation: Some(_),
                    ..
                })
            )
            .then(|| self.recording.compensation_ready()),
            off_reason: projection.off_reason.clone(),
            last_mode_change: projection.last_mode_change.clone(),
        });
        let mut shared = shared.lock().unwrap();
        if matches!(self.journal, "completed" | "failed" | "timed_out") {
            shared.mailbox.active_run = None;
            shared.mailbox.closing = false;
        }
        shared.run = Some(view);
        if let Some(action) = action {
            shared.last_action = Some(action);
        }
    }

    fn perform(&mut self, action: Action, writer_ready: bool) -> Result<(), String> {
        let direct = match action {
            Action::Arm => {
                if !self.preflight(writer_ready).0 {
                    return Err("Предпусковая проверка не пройдена".into());
                }
                OperatorAction::Arm
            }
            Action::Start { confirmed: true } => OperatorAction::Start,
            Action::Reset { confirmed: true } => OperatorAction::Reset,
            Action::Recover => {
                self.projection.borrow_mut().readings = None;
                return self.recording.recover().map_err(|error| error.to_string());
            }
            Action::Fault => {
                if self.recording.state() != MachineState::Running {
                    return Err("Имитация отказа доступна во время прогона".into());
                }
                self.source.disconnect_next_query();
                return Ok(());
            }
            Action::Change(edit) => {
                if self.recording.state() != MachineState::Running {
                    return Err("Смена параметров доступна во время прогона".into());
                }
                if self.prepared.setup.mode.is_some() {
                    return self
                        .recording
                        .change_operator_body(edit)
                        .map_err(|error| error.to_string());
                }
                if edit.material.is_some() || edit.bridge.is_some() {
                    return Err(
                        "Смена материала или типа моста доступна в режиме виртуального тела".into(),
                    );
                }
                let mass = edit
                    .mass_kg
                    .map(|value| Kilograms::new(value).ok_or("Недопустимая масса"))
                    .transpose()?;
                let bridge = edit
                    .g_w_per_k
                    .map(|value| WattsPerKelvin::new(value).ok_or("Недопустимая проводимость"))
                    .transpose()?;
                return self
                    .recording
                    .change_a1_parameters(mass, edit.scale, bridge)
                    .map_err(|error| error.to_string());
            }
            Action::Start { confirmed: false } | Action::Reset { confirmed: false } => {
                return Err("Действие требует подтверждения".into());
            }
            Action::Stop => unreachable!("Stop uses the closing path"),
        };
        self.recording
            .operate(direct)
            .map_err(|error| error.to_string())
    }

    fn close(&mut self, runs: &Path, shared: &Mutex<Shared>, stop: Option<Pending>) {
        self.journal = "closing";
        let result = self
            .recording
            .begin_close()
            .map_err(|error| error.to_string());
        let action =
            stop.map(|request| ActionResult::new(request.id, result, request.cancelled_id));
        self.publish(shared, false, action);
        loop {
            match self.recording.poll_close() {
                CloseResult::Pending => thread::park_timeout(IDLE_POLL),
                CloseResult::Completed(completed) => {
                    self.journal = "completed";
                    self.archive_error = archive::save(
                        runs,
                        &self.id,
                        completed,
                        self.recording.output_state(),
                        &self.prepared.setup,
                    )
                    .err();
                    break;
                }
                CloseResult::Failed(error) => {
                    self.journal = "failed";
                    self.archive_error = Some(error.to_string());
                    break;
                }
                CloseResult::TimedOut => {
                    self.journal = "timed_out";
                    break;
                }
            }
        }
        self.publish(shared, false, None);
    }

    fn drive(&mut self, runs: &Path, shared: &Mutex<Shared>, request_id: u64) {
        let mut next_tick = Instant::now();
        loop {
            let writer = self.recording.poll();
            let now = Instant::now();
            let recording = writer.ready && self.journal == "recording";
            let tick_due = recording && now >= next_tick;
            let (shutdown, pending) = {
                let mut shared = shared.lock().unwrap();
                (
                    shared.shutdown,
                    shared.mailbox.take(now, recording && !tick_due),
                )
            };
            if shutdown {
                self.close(runs, shared, None);
                return;
            }
            if let Some((pending, valid)) = pending {
                if valid.is_ok() && pending.is_stop() {
                    self.close(runs, shared, Some(pending));
                    return;
                }
                let result = valid
                    .map_err(str::to_owned)
                    .and_then(|()| match pending.command {
                        Command::Act(request) => {
                            let recover = matches!(request.action, Action::Recover);
                            let result = self.perform(request.action, writer.ready);
                            if recover && result.is_ok() {
                                next_tick = Instant::now();
                                self.archive_error = None;
                            }
                            result
                        }
                        Command::Create(_) => Err("Сначала завершите текущий прогон".into()),
                    });
                self.publish(
                    shared,
                    writer.ready,
                    Some(ActionResult::new(pending.id, result, pending.cancelled_id)),
                );
                continue;
            }
            if writer.failure.is_some() {
                self.close(runs, shared, None);
                return;
            }
            if self.journal == "opening" && writer.ready {
                self.journal = "recording";
                let failure = self
                    .recording
                    .recover()
                    .err()
                    .map(|error| ActionResult::new(request_id, Err(error.to_string()), None));
                next_tick = Instant::now();
                self.publish(shared, writer.ready, failure);
                continue;
            }
            if tick_due {
                let _ = self.recording.tick(next_tick);
                next_tick = (next_tick + PERIOD).max(now + Duration::from_millis(1));
                self.publish(shared, writer.ready, None);
                continue;
            }
            thread::park_timeout(IDLE_POLL);
        }
    }
}

fn owner_loop(runs: &Path, shared: &Mutex<Shared>) {
    loop {
        let pending = {
            let mut shared = shared.lock().unwrap();
            if shared.shutdown {
                return;
            }
            shared.mailbox.take(Instant::now(), true)
        };
        let Some((pending, valid)) = pending else {
            thread::park();
            continue;
        };
        if let Err(message) = valid {
            shared.lock().unwrap().last_action =
                Some(ActionResult::new(pending.id, Err(message.into()), None));
            continue;
        }
        let Command::Create(prepared) = pending.command else {
            unreachable!("no run command is valid while idle");
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("a1-{timestamp}-{}-{}", std::process::id(), pending.id);
        shared.lock().unwrap().mailbox.active_run = Some(id.clone());
        if let Err(message) = run_session(runs, shared, id, pending.id, prepared) {
            let mut shared = shared.lock().unwrap();
            shared.mailbox.active_run = None;
            shared.last_action = Some(ActionResult::new(pending.id, Err(message), None));
        }
    }
}

fn run_session(
    runs: &Path,
    shared: &Mutex<Shared>,
    id: String,
    request_id: u64,
    prepared: Arc<Prepared>,
) -> Result<(), String> {
    let source = KeithleySimulator::start().map_err(|error| error.to_string())?;
    let thermometer = LakeShoreSim::spawn().map_err(|error| error.to_string())?;
    let epoch = Instant::now();
    let path = runs.join(&id);
    let mut recording = match prepared.setup.mode_snapshot()? {
        Some(snapshot) => {
            RecordingLoop::new_operator_mode(&source, &thermometer, &id, path, epoch, snapshot)
        }
        None => RecordingLoop::new_a1_configured(
            &source,
            &thermometer,
            &id,
            path,
            epoch,
            &prepared.setup.body,
        ),
    }
    .map_err(|error| error.to_string())?;
    let projection = Rc::new(RefCell::new(Projection::new(&prepared.setup)));
    let observed = Rc::clone(&projection);
    recording
        .observe_accepted(move |receipt, events| observed.borrow_mut().accept(receipt, events));
    let mut run = ActiveRun {
        id,
        prepared,
        source: &source,
        recording,
        epoch,
        projection,
        journal: "opening",
        archive_error: None,
    };
    run.publish(
        shared,
        false,
        Some(ActionResult::new(request_id, Ok(()), None)),
    );
    run.drive(runs, shared, request_id);
    Ok(())
}

fn machine_code(state: MachineState) -> &'static str {
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

fn output_code(state: OutputState) -> &'static str {
    match state {
        OutputState::Unknown => "unknown",
        OutputState::OffVerified => "off_verified",
        OutputState::OnVerified => "on_verified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(run: &str, kind: Action) -> Command {
        Command::Act(ActionRequest {
            run: run.into(),
            action: kind,
        })
    }

    #[test]
    fn priority_stop_cancels_the_full_ordinary_slot() {
        let mut mailbox = Mailbox {
            active_run: Some("current".into()),
            ..Mailbox::default()
        };
        let now = Instant::now();
        mailbox
            .enqueue(1, action("current", Action::Start { confirmed: true }), now)
            .unwrap();
        assert!(mailbox
            .enqueue(2, action("current", Action::Arm), now)
            .is_err());
        mailbox
            .enqueue(3, action("current", Action::Stop), now)
            .unwrap();
        let (stop, outcome) = mailbox.take(now, false).unwrap();
        assert!(outcome.is_ok());
        assert_eq!(stop.id, 3);
        assert_eq!(stop.cancelled_id, Some(1));
        assert!(mailbox.take(now, true).is_none());
    }

    #[test]
    fn wrong_run_and_expired_actions_are_rejected_at_the_owner_boundary() {
        let mut mailbox = Mailbox {
            active_run: Some("current".into()),
            ..Mailbox::default()
        };
        let now = Instant::now();
        assert!(mailbox
            .enqueue(1, action("other", Action::Arm), now)
            .is_err());
        mailbox
            .enqueue(2, action("current", Action::Arm), now)
            .unwrap();
        mailbox.active_run = Some("replacement".into());
        assert!(mailbox.take(now, true).unwrap().1.is_err());
        mailbox
            .enqueue(3, action("replacement", Action::Arm), now)
            .unwrap();
        assert!(mailbox
            .take(now + Duration::from_millis(501), true)
            .unwrap()
            .1
            .is_err());
    }

    #[test]
    fn operator_commands_require_the_typed_shape_and_explicit_confirmation() {
        let mut mailbox = Mailbox {
            active_run: Some("current".into()),
            ..Mailbox::default()
        };
        let unconfirmed = serde_json::from_str::<ActionRequest>(
            r#"{"run":"current","kind":"start","confirmed":false}"#,
        )
        .unwrap();
        assert!(mailbox
            .enqueue(1, Command::Act(unconfirmed), Instant::now())
            .is_err());
        assert!(serde_json::from_str::<ActionRequest>(
            r#"{"run":"current","kind":"reset","confirmed":true,"endpoint":"physical"}"#,
        )
        .is_err());
        assert!(
            serde_json::from_str::<ActionRequest>(r#"{"run":"current","kind":"start"}"#,).is_err()
        );
    }
}
