// FILE: crates/cvtl-safety/src/supervisor.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Gate M4 and ON with M5 and journal health
//   SCOPE: Observations, operator actions, regulation and OFF effects
//   DEPENDS: M-CVTL-CONTROL, M-CVTL-CORE
//   LINKS: M-CVTL-SAFETY
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Supervisor - single-use permits and M5 effects
//   SafetyJournal - bounded synchronous handoff contract
//   SafetyRecord - borrowed safety and control telemetry
//   Effect - ON and OFF requests carrying opaque tokens
// END_MODULE_MAP
//! M5 gate between observations, M4 control and actuator effects.
//! The journal implementation and the device connection belong to the caller.

use std::time::Duration;

use cvtl_control::{Actuation, Controller, HeatPermit, Tick};
pub use cvtl_control::{
    ControlOutput, Equivalence, Fault as ControlFault, Warning as ControlWarning,
};
use cvtl_core::units::{Amperes, Watts};

use crate::{
    machine::{MachineError, MachineState, OffEvidence, OutputState, RequestToken, SafetyMachine},
    monitor::{
        MonitorInput, MonitorOutcome, ProfileError, ProtectionProfile, SafetyMonitor, Warning,
    },
    Branch, FaultReason,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorAction {
    Arm,
    Start,
    Stop,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    EnableBoth(RequestToken),
    DisableBoth(RequestToken),
}

#[derive(Debug)]
pub enum SafetyRecord<'a> {
    Observation {
        at: Duration,
        state: MachineState,
        faults: &'a [FaultReason],
        warnings: &'a [Warning],
    },
    Operator {
        at: Duration,
        action: OperatorAction,
    },
    Control {
        at: Duration,
        warning: Option<ControlWarning>,
        fault: Option<ControlFault>,
        output: &'a ControlOutput,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalError;

/// Bounded handoff to the journal. A full or unavailable queue returns an error.
pub trait SafetyJournal {
    fn record(&mut self, record: SafetyRecord<'_>) -> Result<(), JournalError>;
}

#[derive(Debug)]
pub struct SafetyDecision {
    pub permit: HeatPermit,
    /// Soft target reduction, separate from the instantaneous hard permit.
    pub derate_target: Option<Watts>,
    pub effect: Option<Effect>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug)]
pub struct ControlDecision {
    pub currents: Option<[Amperes; 2]>,
    pub effect: Option<Effect>,
}

#[derive(Clone, Copy)]
struct AuthorizedTick {
    at: Duration,
    limit: Watts,
    derate_target: Option<Watts>,
}

pub struct Supervisor {
    machine: SafetyMachine,
    monitor: SafetyMonitor,
    authorized: Option<AuthorizedTick>,
}

impl Supervisor {
    pub fn new(profile: ProtectionProfile) -> Result<Self, ProfileError> {
        Ok(Self {
            machine: SafetyMachine::new(),
            monitor: SafetyMonitor::new(profile)?,
            authorized: None,
        })
    }

    pub fn state(&self) -> MachineState {
        self.machine.state()
    }

    pub fn output_state(&self) -> OutputState {
        self.machine.output_state()
    }

    pub fn fault_reason(&self) -> Option<FaultReason> {
        self.machine.fault_reason()
    }

    pub fn fallback_requested(&self) -> bool {
        self.machine.fallback_requested()
    }

    pub fn connect(&mut self) -> Result<(), MachineError> {
        self.authorized = None;
        self.machine.connect()
    }

    pub fn disconnect(&mut self) {
        self.authorized = None;
        self.machine.disconnect();
    }

    pub fn request_off(&mut self) -> Result<Effect, MachineError> {
        self.authorized = None;
        self.machine.request_off().map(Effect::DisableBoth)
    }

    pub fn confirm_off(
        &mut self,
        request: RequestToken,
        branch: Branch,
        evidence: OffEvidence,
    ) -> Result<(), MachineError> {
        self.machine.confirm_off(request, branch, evidence)
    }

    pub fn confirm_on(
        &mut self,
        request: RequestToken,
        branch: Branch,
    ) -> Result<(), MachineError> {
        self.machine.confirm_on(request, branch)
    }

    /// Check the request immediately before issuing each simulated ON command.
    pub fn can_enable(&self, request: RequestToken) -> bool {
        self.machine.can_enable(request)
    }

    /// An ON request that cannot be confirmed must be followed by OFF on both branches.
    pub fn enable_unconfirmed(&mut self, request: RequestToken) -> Result<Effect, MachineError> {
        if !self.machine.can_enable(request) {
            return Err(MachineError::StaleRequest);
        }
        self.authorized = None;
        Ok(Effect::DisableBoth(
            self.machine.latch_fault(FaultReason::OutputUnconfirmed),
        ))
    }

    pub fn proof_unavailable(&mut self, request: RequestToken) -> Result<(), MachineError> {
        self.authorized = None;
        self.machine.proof_unavailable(request)
    }

    /// Invalidate a pending permit when asynchronous journal health is lost.
    pub fn journal_failed(&mut self) -> Option<Effect> {
        self.trip(FaultReason::JournalFailure)
    }

    pub fn observe(
        &mut self,
        input: &MonitorInput,
        journal: &mut impl SafetyJournal,
    ) -> SafetyDecision {
        self.authorized = None;
        let outcome = match self.inspect(input, journal, true) {
            Ok(outcome) => outcome,
            Err(reason) => return Self::inhibited(self.trip(reason)),
        };
        if self.state() != MachineState::Running {
            return SafetyDecision {
                warnings: outcome.warnings,
                ..Self::inhibited(None)
            };
        }
        let limit = self.monitor.hard_limit(input.cooling_phase);
        self.authorized = Some(AuthorizedTick {
            at: input.monotonic,
            limit,
            derate_target: outcome.derate_power_w,
        });
        SafetyDecision {
            permit: HeatPermit::Limit(limit),
            derate_target: outcome.derate_power_w,
            effect: None,
            warnings: outcome.warnings,
        }
    }

    pub fn operate(
        &mut self,
        action: OperatorAction,
        input: &MonitorInput,
        journal: &mut impl SafetyJournal,
    ) -> Result<SafetyDecision, MachineError> {
        self.authorized = None;
        let record = SafetyRecord::Operator {
            at: input.monotonic,
            action,
        };
        if action == OperatorAction::Stop {
            // OFF takes priority over acquisition and journal success.
            let request = self.machine.request_off()?;
            if journal.record(record).is_err() {
                let effect = self
                    .trip(FaultReason::JournalFailure)
                    .or(Some(Effect::DisableBoth(request)));
                return Ok(Self::inhibited(effect));
            }
            return Ok(Self::inhibited(Some(Effect::DisableBoth(request))));
        }
        let outcome = match self.inspect(input, journal, false) {
            Ok(outcome) => outcome,
            Err(reason) => return Ok(Self::inhibited(self.trip(reason))),
        };
        if journal.record(record).is_err() {
            return Ok(Self::inhibited(self.trip(FaultReason::JournalFailure)));
        }
        let effect = match action {
            OperatorAction::Arm => {
                self.machine.arm()?;
                None
            }
            OperatorAction::Start => Some(Effect::EnableBoth(self.machine.start()?)),
            OperatorAction::Reset => {
                self.machine.reset()?;
                None
            }
            OperatorAction::Stop => unreachable!("handled above"),
        };
        Ok(SafetyDecision {
            warnings: outcome.warnings,
            ..Self::inhibited(effect)
        })
    }

    /// Run M4 under the permit and soft target limit issued for this observation.
    pub fn regulate(
        &mut self,
        at: Duration,
        controller: &mut Controller,
        mut tick: Tick,
        journal: &mut impl SafetyJournal,
    ) -> ControlDecision {
        if self.state() != MachineState::Running {
            return ControlDecision {
                currents: None,
                effect: None,
            };
        }
        let Some(authorization) = self.authorized.filter(|permit| permit.at == at) else {
            return self.control_failure();
        };
        if (tick.now.get() - at.as_secs_f64()).abs() > 1e-9 {
            return self.control_failure();
        }
        tick.permit = HeatPermit::Limit(authorization.limit);
        match controller.tick_with_target_limit(tick, authorization.derate_target) {
            Ok(output) => self.commit_control(at, controller, output, journal),
            Err(_) => self.control_failure(),
        }
    }

    fn commit_control(
        &mut self,
        at: Duration,
        controller: &Controller,
        output: ControlOutput,
        journal: &mut impl SafetyJournal,
    ) -> ControlDecision {
        let authorization = self.authorized.take();
        if self.state() != MachineState::Running {
            return ControlDecision {
                currents: None,
                effect: None,
            };
        }
        let Some(authorization) = authorization.filter(|permit| permit.at == at) else {
            return self.control_failure();
        };
        let written = journal.record(SafetyRecord::Control {
            at,
            warning: output.warning,
            fault: output.fault,
            output: &output,
        });
        if let Some(fault) = output.fault {
            return ControlDecision {
                currents: None,
                effect: self.trip(control_reason(fault)),
            };
        }
        if written.is_err() {
            return ControlDecision {
                currents: None,
                effect: self.trip(FaultReason::JournalFailure),
            };
        }
        match output.action {
            Actuation::Off => match self.machine.stop() {
                Ok(request) => ControlDecision {
                    currents: None,
                    effect: Some(Effect::DisableBoth(request)),
                },
                Err(_) => self.control_failure(),
            },
            Actuation::Drive(currents) => {
                let Some(bounds) = controller.command_bounds(currents) else {
                    return self.control_failure();
                };
                if let Some(reason) = self.monitor.command_violation(bounds, authorization.limit) {
                    return ControlDecision {
                        currents: None,
                        effect: self.trip(reason),
                    };
                }
                if output.limited > authorization.limit {
                    return self.control_failure();
                }
                ControlDecision {
                    currents: Some(currents),
                    effect: None,
                }
            }
        }
    }

    fn inspect(
        &mut self,
        input: &MonitorInput,
        journal: &mut impl SafetyJournal,
        allow_grace: bool,
    ) -> Result<MonitorOutcome, FaultReason> {
        let mut checked = input.clone();
        checked.expect_tick = self.state() == MachineState::Running;
        checked.allow_stale_once &= allow_grace && self.state() == MachineState::Running;
        let outcome = self.monitor.evaluate(&checked);
        let written = journal.record(SafetyRecord::Observation {
            at: checked.monotonic,
            state: self.state(),
            faults: &outcome.faults,
            warnings: &outcome.warnings,
        });
        if let Some(&fault) = outcome.faults.first() {
            return Err(fault);
        }
        written.map_err(|_| FaultReason::JournalFailure)?;
        Ok(outcome)
    }

    fn trip(&mut self, reason: FaultReason) -> Option<Effect> {
        self.authorized = None;
        (self.state() != MachineState::FaultLatched)
            .then(|| Effect::DisableBoth(self.machine.latch_fault(reason)))
    }

    fn control_failure(&mut self) -> ControlDecision {
        self.authorized = None;
        ControlDecision {
            currents: None,
            effect: self.trip(FaultReason::ControllerFailure),
        }
    }

    fn inhibited(effect: Option<Effect>) -> SafetyDecision {
        SafetyDecision {
            permit: HeatPermit::Inhibit,
            derate_target: None,
            effect,
            warnings: Vec::new(),
        }
    }
}

fn control_reason(fault: ControlFault) -> FaultReason {
    match fault {
        ControlFault::BranchLost => FaultReason::BranchLost,
        ControlFault::MeasurementUnavailable => FaultReason::MeasurementUnavailable,
        ControlFault::MeasurementStale => FaultReason::PowerMeasurementStale,
        ControlFault::TemperatureInterlock => FaultReason::HeadInversion,
        ControlFault::Clock => FaultReason::MonotonicClock,
        ControlFault::Calculation => FaultReason::ControllerFailure,
    }
}
