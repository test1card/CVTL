// FILE: crates/cvtl-sim/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Expose local simulation modules
//   SCOPE: Instrument, plant, synthetic head and offline recording
//   DEPENDS: M-CVTL-CORE, M-CVTL-INSTRUMENTS
//   LINKS: M-CVTL-SIM
//   ROLE: BARREL
//   MAP_MODE: SUMMARY
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   head - synthetic thermal step
//   keithley - loopback power source
//   lakeshore - loopback thermometer
//   power_loop - deterministic test plant
//   recording - simulation owner and journal
// END_MODULE_MAP
//! Local simulators for instrument protocol tests.

pub mod head;
pub mod keithley;
pub mod lakeshore;
mod loopback;
pub mod power_loop;
pub mod recording;
