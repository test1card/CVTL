// FILE: crates/cvtl-instruments/src/maisheng.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Reject access to the unverified Maisheng protocol
//   SCOPE: The existing source trait without a transport or state
//   DEPENDS: M-CVTL-CORE
//   LINKS: M-CVTL-INSTRUMENTS
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   MaishengStub - unverified source boundary
// END_MODULE_MAP

use std::time::Instant;

use cvtl_core::units::Amperes;

use crate::{
    keithley::{Channel, ChannelConfig, Measurement, OutputState, SourceChannel},
    InstrumentError, Result,
};

/// The protocol is unverified. Every operation refuses access, including OFF.
pub struct MaishengStub;

impl SourceChannel for MaishengStub {
    fn configure(
        &mut self,
        _channel: Channel,
        _config: ChannelConfig,
        _deadline: Instant,
    ) -> Result<()> {
        Err(InstrumentError::Unverified)
    }

    fn set_current(
        &mut self,
        _channel: Channel,
        _current: Amperes,
        _deadline: Instant,
    ) -> Result<()> {
        Err(InstrumentError::Unverified)
    }

    fn measure(&mut self, _channel: Channel, _deadline: Instant) -> Result<Measurement> {
        Err(InstrumentError::Unverified)
    }

    fn output_on(&mut self, _channel: Channel, _deadline: Instant) -> Result<()> {
        Err(InstrumentError::Unverified)
    }

    fn output_off(&mut self, _channel: Channel, _deadline: Instant) -> Result<()> {
        Err(InstrumentError::Unverified)
    }

    fn output_state(&mut self, _channel: Channel, _deadline: Instant) -> Result<OutputState> {
        Err(InstrumentError::Unverified)
    }

    fn errors(&mut self, _deadline: Instant) -> Result<String> {
        Err(InstrumentError::Unverified)
    }

    fn identity(&mut self, _deadline: Instant) -> Result<String> {
        Err(InstrumentError::Unverified)
    }
}
