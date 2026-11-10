//! Deterministic safety state machine. It emits requests; it performs no I/O.
use crate::FaultReason;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
pub use crate::Branch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    Disconnected,
    Observing,
    OutputOffVerified,
    Armed,
    Running,
    Stopping,
    FaultLatched,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    Unknown,
    OffVerified,
    OnVerified,
}
/// The caller validates the instrument response or both measured quantities against
/// the applicable, explicitly configured limits and freshness policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffEvidence {
    ReportedOff,
    MeasuredNearZero,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestToken {
    session: u64,
    generation: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineError {
    InvalidState,
    StaleRequest,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Off,
    On,
}
#[derive(Debug, Clone, Copy)]
struct Pending {
    token: RequestToken,
    action: Action,
    branches: [bool; 2],
}

#[derive(Debug)]
pub struct SafetyMachine {
    state: MachineState,
    output: OutputState,
    connected: bool,
    fault: Option<FaultReason>,
    session: u64,
    generation: u64,
    pending: Option<Pending>,
    fallback: bool,
}
impl Default for SafetyMachine {
    fn default() -> Self {
        Self::new()
    }
}
impl SafetyMachine {
    pub fn new() -> Self {
        Self {
            state: MachineState::Disconnected,
            output: OutputState::Unknown,
            connected: false,
            fault: None,
            session: NEXT_SESSION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
                .expect("safety session generation exhausted"),
            generation: 0,
            pending: None,
            fallback: false,
        }
    }
    pub fn state(&self) -> MachineState {
        self.state
    }
    pub fn output_state(&self) -> OutputState {
        self.output
    }
    pub fn fault_reason(&self) -> Option<FaultReason> {
        self.fault
    }
    /// A true value requests an independent fallback; it does not assert that it worked.
    pub fn fallback_requested(&self) -> bool {
        self.fallback
    }
    fn invalidate(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("safety request generation exhausted");
        self.pending = None;
    }
    fn request(&mut self, action: Action) -> RequestToken {
        self.invalidate();
        let token = RequestToken {
            session: self.session,
            generation: self.generation,
        };
        self.pending = Some(Pending {
            token,
            action,
            branches: [false; 2],
        });
        self.output = OutputState::Unknown;
        token
    }
    pub fn connect(&mut self) -> Result<(), MachineError> {
        if self.connected {
            return Err(MachineError::InvalidState);
        }
        self.invalidate();
        self.connected = true;
        self.output = OutputState::Unknown;
        self.state = if self.fault.is_some() {
            MachineState::FaultLatched
        } else {
            MachineState::Observing
        };
        Ok(())
    }
    pub fn disconnect(&mut self) {
        let active = matches!(self.state, MachineState::Running | MachineState::Stopping)
            || (self.state == MachineState::FaultLatched && self.output == OutputState::Unknown)
            || self
                .pending
                .is_some_and(|pending| pending.action == Action::On);
        if active {
            if self.fault.is_none() {
                self.fault = Some(FaultReason::SourceLinkLost);
            }
            self.fallback = true;
        }
        self.invalidate();
        self.connected = false;
        self.output = OutputState::Unknown;
        self.state = if self.fault.is_some() {
            MachineState::FaultLatched
        } else {
            MachineState::Disconnected
        };
    }
    pub fn request_off(&mut self) -> Result<RequestToken, MachineError> {
        if !self.connected {
            return Err(MachineError::InvalidState);
        }
        if self.fault.is_none()
            && matches!(
                self.state,
                MachineState::Running
                    | MachineState::Armed
                    | MachineState::Stopping
                    | MachineState::OutputOffVerified
            )
        {
            self.state = MachineState::Stopping;
        }
        Ok(self.request(Action::Off))
    }
    fn confirm(
        &mut self,
        token: RequestToken,
        branch: Branch,
        action: Action,
    ) -> Result<bool, MachineError> {
        if !self.connected {
            return Err(MachineError::StaleRequest);
        }
        let pending = self.pending.as_mut().ok_or(MachineError::StaleRequest)?;
        if pending.token != token || pending.action != action {
            return Err(MachineError::StaleRequest);
        }
        pending.branches[match branch {
            Branch::A => 0,
            Branch::B => 1,
        }] = true;
        Ok(pending.branches.iter().all(|confirmed| *confirmed))
    }
    pub fn confirm_off(
        &mut self,
        token: RequestToken,
        branch: Branch,
        _evidence: OffEvidence,
    ) -> Result<(), MachineError> {
        if self.confirm(token, branch, Action::Off)? {
            self.pending = None;
            self.output = OutputState::OffVerified;
            if self.fault.is_none() {
                self.state = MachineState::OutputOffVerified;
            }
        }
        Ok(())
    }
    pub fn arm(&mut self) -> Result<(), MachineError> {
        if !self.connected
            || self.state != MachineState::OutputOffVerified
            || self.output != OutputState::OffVerified
            || self.fault.is_some()
        {
            return Err(MachineError::InvalidState);
        }
        self.state = MachineState::Armed;
        Ok(())
    }
    pub fn start(&mut self) -> Result<RequestToken, MachineError> {
        if !self.connected || self.state != MachineState::Armed || self.pending.is_some() {
            return Err(MachineError::InvalidState);
        }
        Ok(self.request(Action::On))
    }
    /// Check immediately before dispatching an ON request; the caller must serialize
    /// this check with safety events and output dispatch.
    pub fn can_enable(&self, request: RequestToken) -> bool {
        self.connected
            && self.state == MachineState::Armed
            && self.fault.is_none()
            && self
                .pending
                .is_some_and(|pending| pending.action == Action::On && pending.token == request)
    }
    pub fn confirm_on(&mut self, token: RequestToken, branch: Branch) -> Result<(), MachineError> {
        if self.confirm(token, branch, Action::On)? {
            self.pending = None;
            self.output = OutputState::OnVerified;
            self.state = MachineState::Running;
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Result<RequestToken, MachineError> {
        if !matches!(
            self.state,
            MachineState::Armed | MachineState::Running | MachineState::Stopping
        ) || !self.connected
        {
            return Err(MachineError::InvalidState);
        }
        self.state = MachineState::Stopping;
        Ok(self.request(Action::Off))
    }
    /// Always returns a new request for both OFF actions, even if a cause was already latched.
    pub fn latch_fault(&mut self, reason: FaultReason) -> RequestToken {
        if self.fault.is_none() {
            self.fault = Some(reason);
        }
        self.state = MachineState::FaultLatched;
        self.request(Action::Off)
    }
    pub fn proof_unavailable(&mut self, token: RequestToken) -> Result<(), MachineError> {
        let pending = self.pending.ok_or(MachineError::StaleRequest)?;
        if pending.token != token || pending.action != Action::Off {
            return Err(MachineError::StaleRequest);
        }
        self.invalidate();
        self.output = OutputState::Unknown;
        self.fallback = true;
        if self.fault.is_none() {
            self.fault = Some(FaultReason::OutputUnconfirmed);
        }
        self.state = MachineState::FaultLatched;
        Ok(())
    }
    pub fn reset(&mut self) -> Result<(), MachineError> {
        if !self.connected || self.fault.is_none() || self.output != OutputState::OffVerified {
            return Err(MachineError::InvalidState);
        }
        self.invalidate();
        self.fault = None;
        self.fallback = false;
        self.state = MachineState::OutputOffVerified;
        Ok(())
    }
}
