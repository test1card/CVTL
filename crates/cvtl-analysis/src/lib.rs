// FILE: crates/cvtl-analysis/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Expose offline analysis of CVTL data
//   SCOPE: Stage I historical import
//   DEPENDS: M-CVTL-CORE
//   LINKS: M-CVTL-ANALYSIS
//   ROLE: BARREL
//   MAP_MODE: SUMMARY
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   stage_i - import and project the historical transcription
// END_MODULE_MAP
//! Offline analysis of CVTL data.

pub mod stage_i;
