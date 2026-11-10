// FILE: crates/cvtl-store/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Expose Phase A journal boundaries
//   SCOPE: record, session, writer and recovery
//   DEPENDS: M-CVTL-SAFETY
//   LINKS: M-CVTL-STORE
//   ROLE: BARREL
//   MAP_MODE: SUMMARY
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   record - raw DTOs and setup snapshot
//   session - admission and health
//   writer - filesystem worker
//   recovery - verified prefix
// END_MODULE_MAP
//! Recording and verified recovery of Phase A run data.

pub mod record;
pub mod recovery;
pub mod session;
pub mod writer;
