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
//   export_analysis - completion evidence and shared estimate for an export
// END_MODULE_MAP

use std::{
    fs,
    io::{self, ErrorKind, Read},
    path::Path,
};

use cvtl_analysis::m8::{self, Model, Options, Report};
use cvtl_safety::machine::OutputState;
use cvtl_store::record::MAX_RUN_HEADER_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

const RECEIPT_REQUIRED: &str = "Для анализа требуется квитанция завершения completed.json";

enum ReceiptReadError {
    Missing,
    Invalid(String),
    Metadata(io::Error),
    Read(String),
}

impl ReceiptReadError {
    fn reason(self) -> String {
        match self {
            Self::Missing | Self::Metadata(_) => RECEIPT_REQUIRED.into(),
            Self::Invalid(reason) | Self::Read(reason) => reason,
        }
    }
}

fn read_receipt_bytes(dir: &Path) -> Result<Vec<u8>, ReceiptReadError> {
    let path = dir.join("completed.json");
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err(ReceiptReadError::Invalid(RECEIPT_REQUIRED.into())),
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(ReceiptReadError::Missing),
        Err(error) => return Err(ReceiptReadError::Metadata(error)),
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_RUN_HEADER_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| {
            ReceiptReadError::Read(format!(
                "Не удалось прочитать квитанцию завершения ({error})"
            ))
        })?;
    if bytes.len() > MAX_RUN_HEADER_BYTES {
        return Err(ReceiptReadError::Invalid(
            "Квитанция завершения превышает допустимый размер".into(),
        ));
    }
    Ok(bytes)
}

fn parse_receipt(bytes: &[u8]) -> Result<CompletionReceipt, String> {
    serde_json::from_slice::<ReceiptFile>(bytes)
        .map(|file| file.receipt)
        .map_err(|error| format!("Некорректная квитанция завершения ({error})"))
}

fn read_receipt(dir: &Path) -> Result<CompletionReceipt, String> {
    parse_receipt(&read_receipt_bytes(dir).map_err(ReceiptReadError::reason)?)
}

fn verify_completion(archive: &ArchiveReplay, receipt: &CompletionReceipt) -> Result<(), String> {
    if !receipt.matches_raw(archive) {
        return Err("Проверенные записи не совпадают с квитанцией завершения".into());
    }
    if receipt.output_state != ReceiptOutputState::OffVerified
        || !archive.report.is_matched_off()
        || archive.last_verified_seq != receipt.last_seq
    {
        return Err("Для анализа нужны полный совпавший реплей и подтверждённый OFF".into());
    }
    Ok(())
}

fn captured_model(checked: &CheckedThermal) -> Result<&Model, String> {
    if let Some(reason) = &checked.capture_error {
        return Err(reason.clone());
    }
    checked.model.as_ref().map_err(Clone::clone)
}

fn ready_model<'a>(
    checked: &'a CheckedThermal,
    receipt: &CompletionReceipt,
) -> Result<&'a Model, String> {
    verify_completion(&checked.archive, receipt)?;
    captured_model(checked)
}

fn estimate(checked: &CheckedThermal, options: Options, model: Result<&Model, String>) -> Report {
    match model {
        Ok(model) => m8::analyze(model, &checked.intervals, options),
        Err(reason) => Report::not_determined(options.seed, reason),
    }
}

#[derive(Serialize)]
pub(crate) struct ReceiptEvidence {
    pub status: &'static str,
    sha256: Option<String>,
    receipt: Option<CompletionReceipt>,
    reason: Option<String>,
}

pub(crate) fn export_analysis(
    dir: &Path,
    checked: &CheckedThermal,
    options: Options,
) -> Result<(ReceiptEvidence, Report), String> {
    let (sha256, parsed) = match read_receipt_bytes(dir) {
        Ok(bytes) => (
            Some(format!("{:x}", Sha256::digest(&bytes))),
            parse_receipt(&bytes).map_err(ReceiptReadError::Invalid),
        ),
        Err(error) => (None, Err(error)),
    };
    let (status, receipt, reason) = match parsed {
        Ok(receipt) => match verify_completion(&checked.archive, &receipt) {
            Ok(()) => ("verified", Some(receipt), None),
            Err(reason) => ("unconfirmed", Some(receipt), Some(reason)),
        },
        Err(ReceiptReadError::Missing) => ("missing", None, Some(RECEIPT_REQUIRED.into())),
        Err(ReceiptReadError::Invalid(reason)) => ("invalid", None, Some(reason)),
        Err(ReceiptReadError::Metadata(error)) => {
            return Err(format!(
                "Не удалось проверить квитанцию завершения ({error})"
            ));
        }
        Err(ReceiptReadError::Read(reason)) => return Err(reason),
    };
    let model = match &reason {
        Some(reason) => Err(reason.clone()),
        None => captured_model(checked),
    };
    let report = estimate(checked, options, model);
    Ok((
        ReceiptEvidence {
            status,
            sha256,
            receipt,
            reason,
        },
        report,
    ))
}

pub fn inspect_archive(
    dir: &Path,
    receipt: &CompletionReceipt,
) -> Result<(ArchiveReplay, Report), String> {
    let checked = check_thermal(dir)?;
    let report = estimate(&checked, Options::default(), ready_model(&checked, receipt));
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
    let first_model = ready_model(&first, &receipt);
    let Some(pair_dir) = pair else {
        return Ok(estimate(&first, options, first_model));
    };
    let first_model = match first_model {
        Ok(model) => model,
        Err(reason) => return Ok(Report::not_determined(options.seed, reason)),
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
