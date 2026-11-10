// FILE: crates/cvtl-cli/src/lib.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Share the existing CVTL replay with the operator daemon
//   SCOPE: Offline comparison, bounded timeline and accepted tick projection
//   DEPENDS: M-CVTL-STORE, M-CVTL-CONTROL, M-CVTL-SAFETY
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   run_replay - CLI replay with stable output and exit status
//   check_archive - replay report with bounded verified samples
//   ArchiveReplay - replay result and proof of the checked prefix
//   TimelineSample - accepted input and control data for a plot
//   TIMELINE_LIMIT - maximum number of ticks in an operator timeline
//   quantity - convert optional finite quantities for CLI commands
// END_MODULE_MAP

mod replay;

pub use replay::{check_archive, run as run_replay, ArchiveReplay, TimelineSample, TIMELINE_LIMIT};

#[doc(hidden)]
pub fn quantity<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("Неконечное значение поля {field}"))
}
