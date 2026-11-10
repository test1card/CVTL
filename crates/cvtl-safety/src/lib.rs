//! Deterministic M5 protection logic and state machine. Effects are applied by
//! the caller. Phase A uses simulator profiles and simulator outputs only.

pub mod machine;
pub mod monitor;
pub mod supervisor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Branch {
    A,
    B,
}

impl Branch {
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    pub const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultReason {
    BranchLost,
    SourceLinkLost,
    ThermometerLinkLost,
    MeasurementUnavailable,
    PowerMeasurementStale,
    TemperatureStale,
    BranchCurrent(Branch),
    BranchVoltage(Branch),
    BranchPower(Branch),
    TotalPower,
    MissedTick,
    InstrumentQueue,
    ModelFailure,
    JournalFailure,
    MonotonicClock,
    WallClockJump,
    ChannelMismatch,
    PowerMismatch(Branch),
    HeadInversion,
    HeadAbsoluteTemperature,
    HeadRate,
    HeadPower,
    OutputUnconfirmed,
    ControllerFailure,
}
