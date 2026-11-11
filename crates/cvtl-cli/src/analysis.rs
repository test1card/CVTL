// FILE: crates/cvtl-cli/src/analysis.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Analyze one verified local recording or a comparable pair
//   SCOPE: Completion receipt, replayed model intervals and analysis dispatch
//   DEPENDS: M-CVTL-ANALYSIS, M-CVTL-STORE, M-CVTL-SAFETY
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   CompletionReceipt - persisted proof from the journal owner
//   ReceiptOutputState - state witnessed at journal closure
//   analyze_archive - standalone verified numerical result
//   inspect_archive - one verified replay and analysis for the daemon
// END_MODULE_MAP

use std::{fs, io::Read, path::Path};

use cvtl_analysis::m8::{self, Model, Options, Report};
use cvtl_safety::machine::OutputState;
use cvtl_store::record::MAX_RUN_HEADER_BYTES;
use serde::{Deserialize, Serialize};

use crate::replay::{check_thermal, ArchiveReplay, CheckedThermal};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptOutputState {
    Unknown,
    OffVerified,
    OnVerified,
}

impl From<OutputState> for ReceiptOutputState {
    fn from(output: OutputState) -> Self {
        match output {
            OutputState::Unknown => Self::Unknown,
            OutputState::OffVerified => Self::OffVerified,
            OutputState::OnVerified => Self::OnVerified,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionReceipt {
    pub run: String,
    pub generation: u64,
    pub last_seq: u64,
    pub output_state: ReceiptOutputState,
}

impl CompletionReceipt {
    pub fn matches_raw(&self, archive: &ArchiveReplay) -> bool {
        archive.raw_matches_profile
            && self.generation > 0
            && self.last_seq > 0
            && archive.report.run == self.run
            && archive.report.generation == self.generation
            && archive.report.raw_verified_records == self.last_seq
            && archive.report.raw_tail == "frame_boundary"
    }
}

#[derive(Deserialize)]
struct ReceiptFile {
    receipt: CompletionReceipt,
}

fn read_receipt(dir: &Path) -> Result<CompletionReceipt, String> {
    let path = dir.join("completed.json");
    if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err("Для анализа требуется квитанция завершения completed.json".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_RUN_HEADER_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| format!("Не удалось прочитать квитанцию завершения ({error})"))?;
    if bytes.len() > MAX_RUN_HEADER_BYTES {
        return Err("Квитанция завершения превышает допустимый размер".into());
    }
    serde_json::from_slice::<ReceiptFile>(&bytes)
        .map(|file| file.receipt)
        .map_err(|error| format!("Некорректная квитанция завершения ({error})"))
}

fn ready_model<'a>(
    checked: &'a CheckedThermal,
    receipt: &CompletionReceipt,
) -> Result<&'a Model, String> {
    let archive = &checked.archive;
    if !receipt.matches_raw(archive) {
        return Err("Проверенные записи не совпадают с квитанцией завершения".into());
    }
    if receipt.output_state != ReceiptOutputState::OffVerified
        || !archive.report.is_matched_off()
        || archive.last_verified_seq != receipt.last_seq
    {
        return Err("Для анализа нужны полный совпавший реплей и подтверждённый OFF".into());
    }
    if let Some(reason) = &checked.capture_error {
        return Err(reason.clone());
    }
    checked.model.as_ref().map_err(Clone::clone)
}

pub fn inspect_archive(
    dir: &Path,
    receipt: &CompletionReceipt,
) -> Result<(ArchiveReplay, Report), String> {
    let checked = check_thermal(dir)?;
    let options = Options::default();
    let report = match ready_model(&checked, receipt) {
        Ok(model) => m8::analyze(model, &checked.intervals, options),
        Err(reason) => Report::not_determined(options.seed, reason),
    };
    Ok((checked.archive, report))
}

pub fn analyze_archive(
    dir: &Path,
    options: Options,
    pair: Option<&Path>,
) -> Result<Report, String> {
    let receipt = match read_receipt(dir) {
        Ok(receipt) => receipt,
        Err(reason) => return Ok(Report::not_determined(options.seed, reason)),
    };
    let first = check_thermal(dir)?;
    let first_model = match ready_model(&first, &receipt) {
        Ok(model) => model,
        Err(reason) => return Ok(Report::not_determined(options.seed, reason)),
    };
    let Some(pair_dir) = pair else {
        return Ok(m8::analyze(first_model, &first.intervals, options));
    };

    let second_receipt = match read_receipt(pair_dir) {
        Ok(receipt) => receipt,
        Err(reason) => return Ok(Report::not_determined(options.seed, reason)),
    };
    if receipt.run == second_receipt.run && receipt.generation == second_receipt.generation {
        return Ok(Report::not_determined(
            options.seed,
            "Для пары нужны два независимых прогона",
        ));
    }
    let second = check_thermal(pair_dir)?;
    let second_model = match ready_model(&second, &second_receipt) {
        Ok(model) => model,
        Err(reason) => return Ok(Report::not_determined(options.seed, reason)),
    };
    Ok(m8::analyze_pair(
        first_model,
        &first.intervals,
        second_model,
        &second.intervals,
        options,
    ))
}
