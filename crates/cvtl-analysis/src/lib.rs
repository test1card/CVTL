// FILE: crates/cvtl-analysis/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Expose offline analysis of CVTL data
//   SCOPE: Stage I historical import and verified A1 cooling analysis
//   DEPENDS: M-CVTL-CORE, M-CVTL-PHYSICS
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: BARREL
//   MAP_MODE: SUMMARY
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   m8 - estimate cooling power, slopes and uncertainty
//   stage_i - import and project the historical transcription
// END_MODULE_MAP
//! Offline analysis of CVTL data.

pub mod m8;
pub mod stage_i;
