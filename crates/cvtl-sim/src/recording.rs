// FILE: crates/cvtl-sim/src/recording.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Run one offline recording owner
//   SCOPE: Loopback only, M5 permits, raw evidence and independent OFF
//   DEPENDS: M-CVTL-STORE, M-CVTL-SAFETY, M-CVTL-INSTRUMENTS, M-CVTL-CONTROL
//   LINKS: M-CVTL-SIM
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   RecordingLoop - single owner of loopback effects
//   TickReport - measured host timing
//   Error - journal, safety or protocol failure
// END_MODULE_MAP
//! Sequential test-only owner. No endpoint can be provided by a runtime config.

use std::{
    borrow::Cow,
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cvtl_control::{
    allocation::Allocator,
    pi::{PiConfig, PowerSample},
    ControlConfig, Controller, Demand, HeatPermit, Tick, TimedPowerSample,
};
use cvtl_core::units::{Amperes, Ohms, PerSecond, Seconds, Volts, Watts, WattsPerSecond};
use cvtl_envelope::{ChannelSpec, Margins, ResistanceRange};
use cvtl_instruments::{
    keithley::{Channel, ChannelConfig, Keithley, SourceChannel},
    lakeshore::{self, LakeShore218, SensorUnit, TemperatureReading, Thermometer},
    InstrumentError, LoopbackTcp, MessageTransport, MAX_FRAME_BYTES,
};
use cvtl_safety::{
    machine::{MachineError, MachineState, OffEvidence, OutputState, RequestToken},
    monitor::{BranchReading, CoolingPhase, MonitorInput, ProtectionProfile, Temperatures},
    supervisor::{Effect, OperatorAction, Supervisor},
    Branch, FaultReason,
};
use cvtl_store::{
    record::{self as raw, Command, Correlation, Event, Quality, Snapshot, ThermalPoint},
    session::{fault_issue, CloseResult, Failure, Receipt, Session, Status},
};

use crate::{keithley::KeithleySimulator, lakeshore::LakeShoreSim};

pub const PERIOD: Duration = Duration::from_millis(100);
const COMMAND_LIMIT: Duration = Duration::from_millis(250);
const CHANNELS: [Channel; 2] = [Channel::A, Channel::B];
const RAW_CHANNELS: [raw::Channel; 2] = [raw::Channel::A, raw::Channel::B];
const POINTS: [ThermalPoint; 4] = [
    ThermalPoint::Stage1,
    ThermalPoint::Stage2,
    ThermalPoint::Tf,
    ThermalPoint::Jacket,
];
const MAX_TRACE: usize = 128;
const DEMAND_W: f64 = 0.75;
const MARGIN: f64 = 0.9;
const SLEW_W_PER_S: f64 = 10.0;
const SETUP_YAML: &str = include_str!("../../../config/simulator.yaml");

#[derive(Debug)]
pub enum Error {
    Journal(Failure),
    Machine(MachineError),
    Instrument(InstrumentError),
    Safety(FaultReason),
    StaleTick,
}
impl From<MachineError> for Error {
    fn from(error: MachineError) -> Self {
        Self::Machine(error)
    }
}
impl From<InstrumentError> for Error {
    fn from(error: InstrumentError) -> Self {
        Self::Instrument(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Прогон RECORDING: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy)]
pub struct TickReport {
    pub started: Instant,
    pub missed: bool,
    pub command_latency: Option<Duration>,
    pub measured_w: Option<f64>,
}

#[derive(Default)]
struct Trace {
    events: Vec<Event<'static>>,
    overflow: bool,
}

struct Traced {
    transport: LoopbackTcp,
    source: &'static str,
    epoch: Instant,
    trace: Rc<RefCell<Trace>>,
}

impl Traced {
    fn capture(
        &self,
        command: &str,
        started: Instant,
        sent: bool,
        response: Option<Result<&[u8], &InstrumentError>>,
    ) {
        let mut trace = self.trace.borrow_mut();
        if trace.events.len() + 1 + usize::from(response.is_some()) > MAX_TRACE
            || command.len() > MAX_FRAME_BYTES
            || response
                .as_ref()
                .is_some_and(|result| result.is_ok_and(|data| data.len() > MAX_FRAME_BYTES))
        {
            trace.overflow = true;
            return;
        }
        trace.events.push(Event::Dispatch {
            source: self.source,
            command: Cow::Owned(command.into()),
            status: if sent { "sent" } else { "unknown" },
            at_ns: ns(started.saturating_duration_since(self.epoch)),
        });
        if let Some(result) = response {
            let (bytes, status) = match result {
                Ok(bytes) => (Some(Cow::Owned(bytes.to_vec())), "received"),
                Err(error) => (None, wire_error(error)),
            };
            trace.events.push(Event::Response {
                source: self.source,
                command: Cow::Owned(command.into()),
                bytes,
                status,
                at_ns: ns(self.epoch.elapsed()),
            });
        }
    }
}

impl MessageTransport for Traced {
    fn send(&mut self, command: &str, deadline: Instant) -> cvtl_instruments::Result<()> {
        let started = Instant::now();
        let result = self.transport.send(command, deadline);
        self.capture(command, started, result.is_ok(), None);
        result
    }

    fn query(&mut self, command: &str, deadline: Instant) -> cvtl_instruments::Result<Vec<u8>> {
        let started = Instant::now();
        let result = self.transport.query(command, deadline);
        self.capture(command, started, result.is_ok(), Some(result.as_deref()));
        result
    }

    fn invalidate(&mut self) {
        self.transport.invalidate();
    }
    fn is_usable(&self) -> bool {
        self.transport.is_usable()
    }
}

fn wire_error(error: &InstrumentError) -> &'static str {
    match error {
        InstrumentError::Io(_) => "io",
        InstrumentError::Timeout => "timeout",
        InstrumentError::InvalidResponse(_) => "invalid_response",
        InstrumentError::Disconnected => "disconnected",
        InstrumentError::Unverified => "unverified",
        InstrumentError::InvalidInput(_) => "invalid_input",
        InstrumentError::State(_) => "state",
    }
}

struct Frame {
    monitor: MonitorInput,
    input: raw::Inputs<'static>,
    samples: [Option<TimedPowerSample>; 2],
    measured_started: Option<Instant>,
}

pub struct RecordingLoop<'a> {
    source: &'a KeithleySimulator,
    thermometer: &'a LakeShoreSim,
    keithley: Option<Keithley<Traced>>,
    lake: Option<LakeShore218<Traced>>,
    supervisor: Supervisor,
    controller: Controller,
    journal: Session,
    trace: Rc<RefCell<Trace>>,
    epoch: Instant,
    sequence: u64,
    last_schedule: Option<Instant>,
    last_control: Option<Duration>,
    currents: [Amperes; 2],
    targets_w: [f64; 2],
}

impl<'a> RecordingLoop<'a> {
    pub fn new(
        source: &'a KeithleySimulator,
        thermometer: &'a LakeShoreSim,
        journal: Session,
        epoch: Instant,
    ) -> Self {
        cvtl_core::Config::from_yaml(SETUP_YAML).expect("shipped phase A config");
        Self {
            source,
            thermometer,
            keithley: None,
            lake: None,
            supervisor: Supervisor::new(ProtectionProfile::test_only_example()).unwrap(),
            controller: controller(),
            journal,
            trace: Rc::new(RefCell::new(Trace::default())),
            epoch,
            sequence: 0,
            last_schedule: None,
            last_control: None,
            currents: [a(0.0); 2],
            targets_w: [0.0; 2],
        }
    }

    pub fn snapshot() -> Snapshot {
        let mut yaml = String::from("hardware_enabled: false\nphase_a_config:\n");
        for line in SETUP_YAML.lines() {
            yaml.push_str("  ");
            yaml.push_str(line);
            yaml.push('\n');
        }
        let source = source_config();
        let control = control_config();
        let profile = ProtectionProfile::test_only_example();
        use std::fmt::Write;
        write!(&mut yaml, concat!(
            "test_only:\n  scenario: m6_recording_v1\n  period_ms: {}\n  requested_w: {}\n",
            "  simulators: [keithley_tsp_loopback, lakeshore_218_loopback]\n",
            "  temperature_channels: {{stage1: {}, stage2: {}, tf: 3, jacket: 4, sensor_unit: volts}}\n",
            "  source: {{limit_v: {}, limit_i: {}, limit_p: {}, source_range_i: {}, measure_range_i: {}, measure_range_v: {}, nplc: {}}}\n",
            "  control: {{kp: {}, ki_per_s: {}, total_limit_w: {}, max_sample_age_s: {}, margin: {}, slew_w_per_s: {}}}\n",
        ), PERIOD.as_millis(), DEMAND_W, profile.stage1_channel, profile.stage2_channel,
        source.limit_v.get(), source.limit_i.get(),
        source.limit_p.get(), source.source_range_i.get(), source.measure_range_i.get(), source.measure_range_v.get(), source.nplc,
        control.pi[0].kp, control.pi[0].ki.get(), control.total_limit.get(), control.max_sample_age.get(), MARGIN, SLEW_W_PER_S).unwrap();
        let pi = control.pi[0];
        let channel = control.branches[0];
        let head = profile.head;
        write!(&mut yaml, concat!(
            "  pi: {{integral_limit_w: {}, r_initial_ohm: {}, r_min_ohm: {}, r_max_ohm: {}, r_tau_s: {}, min_current_a: {}, quantum: null, derivative: null}}\n",
            "  heater: {{loop_ohm: {}, limit_w: {}}}\n",
            "  safety: {{profile: m5_test_only_example, max_current_a: {:?}, max_voltage_v: {:?}, max_branch_power_w: {:?}, ",
            "max_total_power_w: {}, phase_power_limits_w: {:?}, max_temperature_age_ms: {}, max_tick_gap_ms: {}, ",
            "tracking_tolerance_w: {}, tracking_duration_ms: {}, wall_jump_tolerance_s: {}}}\n",
            "  head: {{inversion_warning_k: {}, inversion_derate_k: {}, inversion_trip_k: {}, ",
            "temperature_warning_k: {}, temperature_derate_k: {}, temperature_trip_k: {}, min_temperature_k: {}, ",
            "derated_power_w: {}, max_rate_k_per_s: {}}}\n",
        ),
        pi.integral_limit.get(), pi.resistance_initial.get(), pi.resistance_min.get(),
        pi.resistance_max.get(), pi.resistance_tau.get(), pi.min_current.get(),
        channel.loop_resistance.get(), channel.heater_limit.unwrap().get(),
        profile.max_current_a.map(Amperes::get), profile.max_voltage_v.map(Volts::get),
        profile.max_branch_power_w.map(Watts::get), profile.max_total_power_w.get(),
        profile.phase_power_limits_w.map(Watts::get), profile.max_temperature_age.as_millis(),
        profile.max_tick_gap.as_millis(), profile.tracking_tolerance_w.get(),
        profile.tracking_duration.as_millis(), profile.wall_jump_tolerance_s.get(),
        head.inversion_warning_k.get(), head.inversion_derate_k.get(), head.inversion_trip_k.get(),
        head.temperature_warning_k.get(), head.temperature_derate_k.get(), head.temperature_trip_k.get(),
        head.min_temperature_k.get(), head.derated_power_w.get(), head.max_rate_k_per_s.get()).unwrap();
        Snapshot {
            config_yaml: yaml,
            scenario: "test_only_m6_recording_v1".into(),
            references: Vec::new(),
        }
    }

    pub fn state(&self) -> MachineState {
        self.supervisor.state()
    }
    pub fn output_state(&self) -> OutputState {
        self.supervisor.output_state()
    }
    pub fn fault_reason(&self) -> Option<FaultReason> {
        self.supervisor.fault_reason()
    }
    pub fn fallback_requested(&self) -> bool {
        self.supervisor.fallback_requested()
    }

    /// Polling during the scheduler's idle part makes a writer failure an M5
    /// event even if the next control tick has not started.
    pub fn poll(&mut self) -> Status {
        let status = self.journal.poll();
        if status.failure.is_some() {
            self.journal_off(Correlation::Run);
        }
        status
    }

    pub fn recover(&mut self) -> Result<(), Error> {
        let status = self.journal.poll();
        if !status.ready && self.keithley.is_none() {
            return Err(Error::Journal(status.failure.unwrap_or(Failure::NotReady)));
        }
        if self.keithley.is_some() {
            let _ = self.stop(Correlation::Run);
            self.supervisor.disconnect();
        }
        self.lake = None;
        self.keithley = Some(Keithley::new(
            self.transport(self.source.addr(), "keithley")?,
        ));
        self.supervisor.connect()?;
        let Effect::DisableBoth(request) = self.supervisor.request_off()? else {
            unreachable!()
        };
        let recovered = self.keithley.as_mut().unwrap().recover(deadline());
        self.finish_off(request, recovered.is_ok())?;
        if let Err(error) = recovered {
            self.best_effort_output(Correlation::Run);
            return Err(error.into());
        }
        for channel in CHANNELS {
            if let Err(error) =
                self.keithley
                    .as_mut()
                    .unwrap()
                    .configure(channel, source_config(), deadline())
            {
                self.source_failure(Correlation::Run);
                return Err(error.into());
            }
        }
        self.lake = Some(LakeShore218::new(
            self.transport(self.thermometer.addr(), "lakeshore")?,
        ));
        self.controller.reset_fault();
        self.last_control = None;
        self.last_schedule = None;
        self.currents = [a(0.0); 2];
        self.targets_w = [0.0; 2];
        self.record_output(Correlation::Run)?;
        Ok(())
    }

    pub fn operate(&mut self, action: OperatorAction) -> Result<(), Error> {
        let id = self.next_id();
        let correlation = Correlation::Request(id);
        if action == OperatorAction::Stop {
            return self.stop(correlation);
        }
        self.require_journal(correlation)?;
        let frame = self.acquire(Instant::now(), Duration::ZERO, false);
        let input = self.record_inputs(correlation, frame.input, &frame.monitor)?;
        let decision = self.supervisor.operate(
            action,
            &frame.monitor,
            &mut self.journal.journal(correlation),
        )?;
        match decision.effect {
            Some(Effect::DisableBoth(request)) => self.apply_off(request, correlation)?,
            Some(Effect::EnableBoth(request)) => self.enable(request, input, correlation)?,
            None => {}
        }
        self.require_no_safety_fault()?;
        if action == OperatorAction::Reset {
            self.controller.reset_fault();
            self.last_control = None;
        }
        Ok(())
    }

    pub fn tick(&mut self, scheduled: Instant) -> Result<TickReport, Error> {
        if self.last_schedule.is_some_and(|last| scheduled <= last) {
            let _ = self.stop(Correlation::Run);
            return Err(Error::StaleTick);
        }
        self.last_schedule = Some(scheduled);
        let started = Instant::now();
        let missed = started.saturating_duration_since(scheduled) >= PERIOD;
        let id = self.next_id();
        let correlation = Correlation::Tick(id);
        self.require_journal(correlation)?;
        let mut frame = self.acquire(scheduled, PERIOD, missed);
        let dt = self
            .last_control
            .map_or(PERIOD, |previous| frame.monitor.monotonic - previous);
        frame.input.dt_ns = ns(dt);
        let input = self.record_inputs(correlation, frame.input, &frame.monitor)?;
        let measured_w = frame.samples[0].zip(frame.samples[1]).map(|(a, b)| {
            a.sample.current.get() * a.sample.voltage.get()
                + b.sample.current.get() * b.sample.voltage.get()
        });
        let decision = self
            .supervisor
            .observe(&frame.monitor, &mut self.journal.journal(correlation));
        if let Some(Effect::DisableBoth(request)) = decision.effect {
            self.apply_off(request, correlation)?;
        }
        self.require_no_safety_fault()?;

        let mut command_latency = None;
        if self.state() == MachineState::Running {
            let now = frame.monitor.monotonic;
            let mut journal = self.journal.journal(correlation);
            let result = self.supervisor.regulate(
                now,
                &mut self.controller,
                Tick {
                    demand: Demand::Power(w(DEMAND_W)),
                    permit: HeatPermit::Inhibit,
                    branches_available: frame.samples.map(|value| value.is_some()),
                    measurements: frame.samples,
                    now: s(now.as_secs_f64()),
                    dt: s(dt.as_secs_f64()),
                },
                &mut journal,
            );
            self.last_control = Some(now);
            let targets = journal.targets_w();
            if let Some(Effect::DisableBoth(request)) = result.effect {
                self.apply_off(request, correlation)?;
            }
            self.require_no_safety_fault()?;
            if let Some(currents) = result.currents {
                let ack = self.drive(currents, input, correlation)?;
                command_latency = frame.measured_started.map(|at| ack.duration_since(at));
                self.targets_w = targets.expect("accepted control output has targets");
            }
        }
        Ok(TickReport {
            started,
            missed,
            command_latency,
            measured_w,
        })
    }

    pub fn begin_close(&mut self) -> Result<(), Error> {
        let result = if self.keithley.is_some() {
            self.stop(Correlation::Run)
        } else {
            Ok(())
        };
        self.journal.begin_close();
        result
    }

    pub fn poll_close(&mut self) -> CloseResult {
        self.journal.poll_close()
    }

    fn transport(
        &self,
        addr: std::net::SocketAddr,
        source: &'static str,
    ) -> cvtl_instruments::Result<Traced> {
        Ok(Traced {
            transport: LoopbackTcp::connect(addr, COMMAND_LIMIT)?,
            source,
            epoch: self.epoch,
            trace: self.trace.clone(),
        })
    }

    fn next_id(&mut self) -> u64 {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("simulator correlation exhausted");
        self.sequence
    }

    fn acquire(&mut self, scheduled: Instant, dt: Duration, missed_tick: bool) -> Frame {
        let (iv, iv_quality) = match &mut self.keithley {
            Some(driver) => {
                let results = CHANNELS.map(|channel| driver.measure(channel, deadline()));
                let quality = results
                    .each_ref()
                    .map(|result| quality(result.as_ref().map(|_| ())));
                (results.map(Result::ok), quality)
            }
            None => ([None; 2], [Quality::Missing; 2]),
        };
        let (temperatures, temperature_quality) = match &mut self.lake {
            Some(driver) => {
                let results = [1, 2, 3, 4].map(|channel| {
                    driver.read(
                        lakeshore::Channel::new(channel).unwrap(),
                        SensorUnit::Volts,
                        deadline(),
                    )
                });
                let quality = results
                    .each_ref()
                    .map(|result| quality(result.as_ref().map(|_| ())));
                (results.map(Result::ok), quality)
            }
            None => ([None; 4], [Quality::Missing; 4]),
        };
        let now = self.epoch.elapsed();
        let mut monitor = self.empty_monitor(now);
        monitor.source_connected = iv.iter().all(Option::is_some);
        monitor.thermometer_connected = temperatures.iter().all(Option::is_some);
        monitor.missed_tick = missed_tick;
        monitor.branches = std::array::from_fn(|index| {
            iv[index].map(|reading| BranchReading {
                current_a: reading.current_a,
                voltage_v: reading.voltage_v,
                command_power_w: w(self.targets_w[index]),
            })
        });
        if let [Some(stage1), Some(stage2), ..] = temperatures {
            monitor.temperatures = Some(Temperatures {
                stage1_k: stage1.kelvin,
                stage2_k: stage2.kelvin,
                measured_at: self
                    .since_epoch(temperature_time(stage1).min(temperature_time(stage2))),
                stage1_channel: stage1.channel.number(),
                stage2_channel: stage2.channel.number(),
            });
        }
        let samples = iv.map(|measurement| {
            measurement.map(|reading| TimedPowerSample {
                at: s(self.since_epoch(reading.started).as_secs_f64()),
                sample: PowerSample {
                    voltage: reading.voltage_v,
                    current: reading.current_a,
                },
            })
        });
        let input = raw::Inputs {
            scheduled_ns: ns(self.since_epoch(scheduled)),
            measured_ns: ns(now),
            wall_time_s: monitor.wall_time_s.get(),
            dt_ns: ns(dt),
            power: std::array::from_fn(|index| raw::PowerSample {
                channel: RAW_CHANNELS[index],
                quality: iv_quality[index],
                current_a: iv[index].map(|value| value.current_a.get()),
                voltage_v: iv[index].map(|value| value.voltage_v.get()),
                target_w: Some(self.targets_w[index]),
                read_started_ns: iv[index].map(|value| ns(self.since_epoch(value.started))),
                read_finished_ns: iv[index].map(|value| ns(self.since_epoch(value.finished))),
            }),
            temperatures: std::array::from_fn(|index| raw::TemperatureSample {
                point: POINTS[index],
                channel: Some(index as u8 + 1),
                quality: temperature_quality[index],
                temperature_k: temperatures[index].map(|value| value.kelvin.get()),
                read_started_ns: temperatures[index]
                    .map(|value| ns(self.since_epoch(value.started))),
                read_finished_ns: temperatures[index]
                    .map(|value| ns(self.since_epoch(value.finished))),
            }),
            demand: raw::Demand::Power {
                requested_w: DEMAND_W,
            },
            source_connected: monitor.source_connected,
            thermometer_connected: monitor.thermometer_connected,
            missed_tick,
            allow_stale_once: monitor.allow_stale_once,
            expect_tick: monitor.expect_tick,
            queue_fault: false,
            model_valid: true,
            journal_ok: monitor.journal_ok,
            cooling_phase: "cooling",
        };
        Frame {
            monitor,
            input,
            samples,
            measured_started: iv[0].map(|value| value.started),
        }
    }

    fn empty_monitor(&mut self, now: Duration) -> MonitorInput {
        MonitorInput {
            monotonic: now,
            wall_time_s: s(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64()),
            branches: [None; 2],
            branch_channels: Branch::ALL,
            temperatures: None,
            allow_stale_once: false,
            source_connected: false,
            thermometer_connected: false,
            missed_tick: false,
            expect_tick: self.state() == MachineState::Running,
            queue_fault: false,
            model_valid: true,
            journal_ok: self.journal.poll().ready,
            cooling_phase: CoolingPhase::Cooling,
        }
    }

    fn since_epoch(&self, at: Instant) -> Duration {
        at.saturating_duration_since(self.epoch)
    }

    fn take_trace(&self) -> Result<Vec<Event<'static>>, Failure> {
        let trace = std::mem::take(&mut *self.trace.borrow_mut());
        if trace.overflow {
            Err(Failure::Record(raw::RecordError::Size))
        } else {
            Ok(trace.events)
        }
    }

    fn record_inputs(
        &mut self,
        correlation: Correlation,
        frame: raw::Inputs<'static>,
        monitor: &MonitorInput,
    ) -> Result<Receipt, Error> {
        let admitted = self.take_trace().and_then(|mut batch| {
            batch.push(Event::Inputs {
                frame: Box::new(frame),
            });
            self.journal.accept(correlation, &batch)
        });
        admitted.map_err(|failure| {
            // The acquired sample may already carry a higher-priority M5 fault.
            let mut denied = monitor.clone();
            denied.journal_ok = false;
            let decision = self
                .supervisor
                .observe(&denied, &mut self.journal.journal(correlation));
            if let Some(Effect::DisableBoth(request)) = decision.effect {
                let _ = self.apply_off(request, correlation);
            }
            Error::Journal(failure)
        })
    }

    fn admit(&mut self, correlation: Correlation, events: &[Event<'_>]) -> Result<Receipt, Error> {
        self.journal
            .accept(correlation, events)
            .map_err(|failure| self.journal_error(correlation, failure))
    }

    fn intents(
        &mut self,
        correlation: Correlation,
        commands: [Command; 2],
    ) -> Result<Receipt, Error> {
        let at_ns = ns(self.epoch.elapsed());
        let events = std::array::from_fn::<_, 2, _>(|index| Event::CommandIntent {
            channel: RAW_CHANNELS[index],
            command: commands[index],
            at_ns,
        });
        self.admit(correlation, &events)
    }

    fn gate(
        &mut self,
        input: Receipt,
        intent: Receipt,
        correlation: Correlation,
    ) -> Result<(), Error> {
        if input.correlation != correlation
            || intent.correlation != correlation
            || input.generation != intent.generation
            || input.last_seq >= intent.first_seq
        {
            return Err(self.journal_error(correlation, Failure::Protocol));
        }
        self.require_journal(correlation)
    }

    fn enable(
        &mut self,
        request: RequestToken,
        input: Receipt,
        correlation: Correlation,
    ) -> Result<(), Error> {
        // OFF leaves the instrument's programmed level intact. Prove zero
        // on both branches before enabling either one.
        for (command, preset) in [(Command::ZeroPreset, true), (Command::On, false)] {
            let intent = self.intents(correlation, [command; 2])?;
            for (branch, channel) in Branch::ALL.into_iter().zip(CHANNELS) {
                self.gate(input, intent, correlation)?;
                if !self.supervisor.can_enable(request) {
                    return Err(Error::Machine(MachineError::StaleRequest));
                }
                let driver = self
                    .keithley
                    .as_mut()
                    .ok_or(InstrumentError::Disconnected)?;
                let outcome = if preset {
                    driver.set_current(channel, a(0.0), deadline())
                } else {
                    driver.output_on(channel, deadline())
                };
                if let Err(error) = outcome {
                    let Effect::DisableBoth(off) = self.supervisor.enable_unconfirmed(request)?
                    else {
                        unreachable!()
                    };
                    let _ = self.apply_off(off, correlation);
                    return Err(error.into());
                }
                if !preset {
                    self.supervisor.confirm_on(request, branch)?;
                }
                self.record_transport(correlation)?;
            }
        }
        self.controller.reset_fault();
        self.last_control = None;
        self.record_output(correlation)?;
        self.require_journal(correlation)
    }

    fn drive(
        &mut self,
        currents: [Amperes; 2],
        input: Receipt,
        correlation: Correlation,
    ) -> Result<Instant, Error> {
        let intent = self.intents(
            correlation,
            currents.map(|current| Command::Drive {
                current_a: current.get(),
            }),
        )?;
        let order = if currents[0] > self.currents[0] && currents[1] < self.currents[1] {
            [1, 0]
        } else {
            [0, 1]
        };
        let mut last_ack = None;
        for index in order {
            self.gate(input, intent, correlation)?;
            if self.state() != MachineState::Running {
                return Err(Error::Machine(MachineError::InvalidState));
            }
            let outcome = self
                .keithley
                .as_mut()
                .ok_or(InstrumentError::Disconnected)?
                .set_current(CHANNELS[index], currents[index], deadline());
            if let Err(error) = outcome {
                self.source_failure(correlation);
                return Err(error.into());
            }
            last_ack = Some(Instant::now());
            self.currents[index] = currents[index];
            self.record_transport(correlation)?;
        }
        self.require_journal(correlation)?;
        Ok(last_ack.expect("two confirmed branches"))
    }

    fn require_journal(&mut self, correlation: Correlation) -> Result<(), Error> {
        let status = self.journal.poll();
        if let Some(failure) = status.failure {
            return Err(self.journal_error(correlation, failure));
        }
        if !status.ready {
            return Err(Error::Journal(Failure::NotReady));
        }
        Ok(())
    }

    fn require_no_safety_fault(&self) -> Result<(), Error> {
        self.fault_reason()
            .map_or(Ok(()), |fault| Err(Error::Safety(fault)))
    }

    fn journal_error(&mut self, correlation: Correlation, failure: Failure) -> Error {
        self.journal_off(correlation);
        Error::Journal(failure)
    }

    fn journal_off(&mut self, correlation: Correlation) {
        if let Some(Effect::DisableBoth(request)) = self.supervisor.journal_failed() {
            let _ = self.apply_off(request, correlation);
        }
    }

    fn source_failure(&mut self, correlation: Correlation) {
        // A failed transport has invalidated proof. A successful future readback
        // belongs to a fresh request after explicit recovery.
        self.supervisor.disconnect();
        if let Some(driver) = self.keithley.as_mut() {
            let _ = driver.disable_both(deadline());
        }
        self.best_effort_output(correlation);
    }

    fn record_transport(&mut self, correlation: Correlation) -> Result<(), Error> {
        let events = self
            .take_trace()
            .map_err(|failure| self.journal_error(correlation, failure))?;
        if !events.is_empty() {
            self.admit(correlation, &events)?;
        }
        Ok(())
    }

    fn record_output(&mut self, correlation: Correlation) -> Result<(), Error> {
        let mut events = self
            .take_trace()
            .map_err(|failure| self.journal_error(correlation, failure))?;
        events.push(self.output_evidence());
        self.admit(correlation, &events)?;
        Ok(())
    }

    fn best_effort_output(&mut self, correlation: Correlation) {
        if let Ok(mut events) = self.take_trace() {
            events.push(self.output_evidence());
            let _ = self.journal.accept(correlation, &events);
        }
    }

    fn output_evidence(&self) -> Event<'static> {
        Event::OutputEvidence {
            at_ns: ns(self.epoch.elapsed()),
            state: match self.output_state() {
                OutputState::Unknown => "unknown",
                OutputState::OffVerified => "off_verified",
                OutputState::OnVerified => "on_verified",
            },
            fault: self.fault_reason().map(|fault| fault_issue(fault).code),
            fallback_requested: self.fallback_requested(),
        }
    }

    fn stop(&mut self, correlation: Correlation) -> Result<(), Error> {
        // No new measurements and no synchronous file operation in the OFF path.
        let frame = self.empty_monitor(self.epoch.elapsed());
        let effect = self.supervisor.operate(
            OperatorAction::Stop,
            &frame,
            &mut self.journal.journal(correlation),
        );
        match effect {
            Ok(decision) => {
                if let Some(Effect::DisableBoth(request)) = decision.effect {
                    self.apply_off(request, correlation)?;
                }
                Ok(())
            }
            Err(error) if self.output_state() == OutputState::Unknown => {
                if let Some(driver) = &mut self.keithley {
                    let _ = driver.disable_both(deadline());
                }
                self.best_effort_output(correlation);
                Err(error.into())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn apply_off(&mut self, request: RequestToken, correlation: Correlation) -> Result<(), Error> {
        let at_ns = ns(self.epoch.elapsed());
        let intents = RAW_CHANNELS.map(|channel| Event::CommandIntent {
            channel,
            command: Command::Off,
            at_ns,
        });
        let _ = self.journal.accept(correlation, &intents);
        let result = self
            .keithley
            .as_mut()
            .ok_or(InstrumentError::Disconnected)
            .and_then(|driver| driver.disable_both(deadline()));
        self.finish_off(request, result.is_ok())?;
        self.best_effort_output(correlation);
        result.map_err(Error::from)
    }

    fn finish_off(&mut self, request: RequestToken, verified: bool) -> Result<(), Error> {
        if verified {
            for branch in Branch::ALL {
                self.supervisor
                    .confirm_off(request, branch, OffEvidence::ReportedOff)?;
            }
            self.currents = [a(0.0); 2];
            self.targets_w = [0.0; 2];
        } else {
            self.supervisor.proof_unavailable(request)?;
        }
        Ok(())
    }
}

impl Drop for RecordingLoop<'_> {
    fn drop(&mut self) {
        if self.keithley.is_some() && self.output_state() != OutputState::OffVerified {
            let _ = self.stop(Correlation::Run);
        }
    }
}

fn quality(result: Result<(), &InstrumentError>) -> Quality {
    match result {
        Ok(()) => Quality::Measured,
        Err(InstrumentError::InvalidResponse(_)) => Quality::Invalid,
        Err(_) => Quality::Missing,
    }
}

fn temperature_time(reading: TemperatureReading) -> Instant {
    reading.measurement_time.unwrap_or(reading.started)
}
fn ns(value: Duration) -> u64 {
    value
        .as_nanos()
        .try_into()
        .expect("simulator clock exhausted")
}
fn deadline() -> Instant {
    Instant::now() + COMMAND_LIMIT
}
fn a(value: f64) -> Amperes {
    Amperes::new(value).unwrap()
}
fn w(value: f64) -> Watts {
    Watts::new(value).unwrap()
}
fn v(value: f64) -> Volts {
    Volts::new(value).unwrap()
}
fn s(value: f64) -> Seconds {
    Seconds::new(value).unwrap()
}
fn r(value: f64) -> Ohms {
    Ohms::new(value).unwrap()
}

fn source_config() -> ChannelConfig {
    ChannelConfig {
        limit_v: v(10.0),
        limit_i: a(1.0),
        limit_p: w(4.0),
        source_range_i: a(1.0),
        measure_range_i: a(1.0),
        measure_range_v: v(40.0),
        nplc: 1.0,
    }
}

fn control_config() -> ControlConfig {
    let channel = ChannelSpec {
        resistance: ResistanceRange::new(r(10.0), r(11.0)).unwrap(),
        loop_resistance: r(0.0),
        heater_limit: Some(w(4.0)),
    };
    let pi = PiConfig {
        kp: 0.3,
        ki: PerSecond::new(1.0).unwrap(),
        integral_limit: w(2.0),
        resistance_initial: r(10.0),
        resistance_min: r(10.0),
        resistance_max: r(11.0),
        resistance_tau: s(0.3),
        min_current: a(0.001),
        current_quantum: None,
        derivative: None,
    };
    ControlConfig {
        pi: [pi; 2],
        branches: [channel; 2],
        margins: Margins::new(MARGIN, MARGIN).unwrap(),
        total_limit: w(6.0),
        max_sample_age: s(0.5),
        temperature_hold: None,
    }
}

fn controller() -> Controller {
    let rate = WattsPerSecond::new(SLEW_W_PER_S).unwrap();
    Controller::new(
        control_config(),
        Allocator::new(None, rate, rate, [rate; 2], [rate; 2]).unwrap(),
    )
    .unwrap()
}
