// FILE: crates/cvtl-sim/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Expose local simulation modules
//   SCOPE: Instrument, plant, synthetic head and offline recording
//   DEPENDS: M-CVTL-CORE, M-CVTL-INSTRUMENTS, M-CVTL-MATERIALS, M-CVTL-PHYSICS
//   LINKS: M-CVTL-SIM
//   ROLE: BARREL
//   MAP_MODE: SUMMARY
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   a1 - closed-loop thermal scenario
//   head - synthetic thermal step
//   historical_head - Stage I characteristic and synthetic continuation
//   keithley - loopback power source
//   lakeshore - loopback thermometer
//   maisheng - in-memory test-only source interface
//   modes - shared test-only mode settings and demand calculation
//   power_loop - deterministic test plant
//   recording - simulation owner and journal
// END_MODULE_MAP
//! Local simulators for instrument protocol tests.

mod a1;
pub mod head;
pub mod historical_head;
pub mod keithley;
pub mod lakeshore;
mod loopback;
pub mod maisheng;
pub mod modes;
pub mod power_loop;
pub mod recording;
