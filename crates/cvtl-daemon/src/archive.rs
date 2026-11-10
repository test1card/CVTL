// FILE: crates/cvtl-daemon/src/archive.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Qualify a local completed archive for operator review
//   SCOPE: Durable completion receipts and bounded offline replay
//   DEPENDS: M-CVTL-CLI, M-CVTL-SAFETY, M-CVTL-STORE
//   LINKS: M-CVTL-DAEMON
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   save - persist the completion receipt after writer closure
//   list - find receipts of local completed runs
//   review - compare a receipt to a verified raw prefix
// END_MODULE_MAP

use cvtl_cli::{check_archive, ArchiveReplay};
use cvtl_safety::machine::OutputState;
use cvtl_store::{record::MAX_RUN_HEADER_BYTES, session::Completed};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::scenario::{FixturePart, Setup, Wire};

const RECEIPT_FILE: &str = "completed.json";
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Serialize)]
pub(crate) struct Receipt {
    run: String,
    generation: u64,
    last_seq: u64,
    output_state: ReceiptOutputState,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptOutputState {
    Unknown,
    OffVerified,
    OnVerified,
}

#[derive(Deserialize, Serialize)]
struct Diagnostics {
    wiring: Vec<Wire>,
    fixture: Vec<FixturePart>,
}

#[derive(Deserialize, Serialize)]
struct SavedReceipt {
    receipt: Receipt,
    diagnostics: Diagnostics,
}

#[derive(Serialize)]
pub(crate) struct Review {
    #[serde(flatten)]
    replay: ArchiveReplay,
    receipt_verified: bool,
    #[serde(flatten)]
    saved: SavedReceipt,
}

pub(crate) fn save(
    root: &Path,
    run: &str,
    completed: Completed,
    output: OutputState,
    setup: &Setup,
) -> Result<(), String> {
    let dir = run_dir(root, run)?;
    if completed.generation == 0 {
        return Err("Недопустимое поколение квитанции".into());
    }
    let saved = SavedReceipt {
        receipt: Receipt {
            run: run.into(),
            generation: completed.generation,
            last_seq: completed.last_seq,
            output_state: match output {
                OutputState::Unknown => ReceiptOutputState::Unknown,
                OutputState::OffVerified => ReceiptOutputState::OffVerified,
                OutputState::OnVerified => ReceiptOutputState::OnVerified,
            },
        },
        diagnostics: Diagnostics {
            wiring: setup.wiring.clone(),
            fixture: setup.fixture.clone(),
        },
    };
    let data = serde_json::to_vec(&saved)
        .map_err(|error| format!("Ошибка формирования квитанции ({error})"))?;
    if data.len() > MAX_RUN_HEADER_BYTES {
        return Err("Квитанция превышает допустимый размер".into());
    }
    let pending = dir.join(format!(
        ".completed-{}-{}.tmp",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)
        .map_err(|error| format!("Ошибка создания квитанции ({error})"))?;
    let result = (|| {
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&pending, dir.join(RECEIPT_FILE))?;
        File::open(&dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result.map_err(|error| format!("Ошибка сохранения квитанции ({error})"))
}

pub(crate) fn list(root: &Path) -> Result<Vec<Receipt>, String> {
    let entries =
        fs::read_dir(root).map_err(|error| format!("Ошибка чтения каталога прогонов ({error})"))?;
    let mut receipts = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(run) = name.to_str() else { continue };
        let Ok(dir) = run_dir(root, run) else {
            continue;
        };
        let Ok(saved) = read_receipt(&dir, run) else {
            continue;
        };
        receipts.push(saved.receipt);
    }
    receipts.sort_unstable_by(|a, b| b.run.cmp(&a.run));
    Ok(receipts)
}

pub(crate) fn review(root: &Path, run: &str) -> Result<Review, String> {
    let dir = run_dir(root, run)?;
    let saved = read_receipt(&dir, run)?;
    let replay = check_archive(&dir)?;
    let receipt = &saved.receipt;
    let report = &replay.report;
    let receipt_verified = replay.raw_matches_profile
        && report.run == receipt.run
        && report.generation == receipt.generation
        && report.raw_verified_records == receipt.last_seq
        && report.raw_tail == "frame_boundary";
    Ok(Review {
        replay,
        receipt_verified,
        saved,
    })
}

fn run_dir(root: &Path, run: &str) -> Result<PathBuf, String> {
    if run.is_empty()
        || run.len() > 64
        || !run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("Недопустимый идентификатор прогона".into());
    }
    let dir = root.join(run);
    if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Err("Каталог прогона недоступен".into());
    }
    Ok(dir)
}

fn read_receipt(dir: &Path, run: &str) -> Result<SavedReceipt, String> {
    let path = dir.join(RECEIPT_FILE);
    if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err("Квитанция завершения прогона недоступна".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_RUN_HEADER_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| format!("Ошибка чтения квитанции ({error})"))?;
    if bytes.len() > MAX_RUN_HEADER_BYTES {
        return Err("Квитанция превышает допустимый размер".into());
    }
    let saved: SavedReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Ошибка разбора квитанции ({error})"))?;
    if saved.receipt.run != run || saved.receipt.generation == 0 {
        return Err("Квитанция относится к другому прогону или поколению".into());
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cvtl_safety::supervisor::OperatorAction;
    use cvtl_sim::{
        keithley::KeithleySimulator, lakeshore::LakeShoreSim, recording::RecordingLoop,
    };
    use cvtl_store::{session::CloseResult, writer::spawn_file};
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    };

    const RUN: &str = "archive-1";

    struct Recording {
        root: PathBuf,
        completed: Completed,
        output: OutputState,
    }

    impl Recording {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cvtl-archive-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let mut fixture = Self {
                root,
                completed: Completed {
                    generation: 0,
                    last_seq: 0,
                },
                output: OutputState::Unknown,
            };
            let source = KeithleySimulator::start().unwrap();
            let lake = LakeShoreSim::spawn().unwrap();
            let epoch = Instant::now();
            let session = spawn_file(
                RUN,
                1,
                fixture.root.join(RUN),
                RecordingLoop::snapshot(),
                move || epoch.elapsed(),
            )
            .unwrap();
            let mut owner = RecordingLoop::new(&source, &lake, session, epoch);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !owner.poll().ready {
                assert!(Instant::now() < deadline);
                thread::park_timeout(Duration::from_millis(2));
            }
            owner.recover().unwrap();
            owner.operate(OperatorAction::Arm).unwrap();
            owner.operate(OperatorAction::Start).unwrap();
            owner.tick(Instant::now()).unwrap();
            owner.tick(Instant::now()).unwrap();
            owner.begin_close().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            fixture.completed = loop {
                match owner.poll_close() {
                    CloseResult::Completed(receipt) => break receipt,
                    CloseResult::Pending => {
                        assert!(Instant::now() < deadline);
                        thread::park_timeout(Duration::from_millis(2));
                    }
                    error => panic!("{error:?}"),
                }
            };
            fixture.output = owner.output_state();
            fixture
        }

        fn save(&self, completed: Completed, output: OutputState) {
            save(&self.root, RUN, completed, output, &Setup::default()).unwrap();
        }

        fn review(&self) -> serde_json::Value {
            serde_json::to_value(review(&self.root, RUN).unwrap()).unwrap()
        }
    }

    impl Drop for Recording {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn archive_requires_a_receipt_and_keeps_off_evidence_separate() {
        let fixture = Recording::new();
        assert!(review(&fixture.root, RUN).is_err());
        assert!(review(&fixture.root, "../archive-1").is_err());
        assert_eq!(
            serde_json::to_value(list(&fixture.root).unwrap()).unwrap(),
            serde_json::json!([])
        );
        fixture.save(fixture.completed, OutputState::Unknown);
        let listed = serde_json::to_value(list(&fixture.root).unwrap()).unwrap();
        assert_eq!(listed[0]["run"], RUN);
        assert_eq!(listed[0]["last_seq"], fixture.completed.last_seq);
        let checked = fixture.review();
        assert_eq!(checked["receipt_verified"], true);
        assert_eq!(checked["receipt"]["output_state"], "unknown");
        assert_eq!(
            checked["report"]["output_state_at_boundary"],
            "off_verified"
        );
        assert_eq!(checked["report"]["historical_completion"], "unknown");
        assert_eq!(checked["last_verified_seq"], fixture.completed.last_seq);
        assert_eq!(checked["samples"].as_array().unwrap().len(), 2);
        assert_eq!(checked["omitted_ticks"], 0);
        assert_eq!(
            checked["diagnostics"]["wiring"].as_array().unwrap().len(),
            2
        );

        std::os::unix::fs::symlink(fixture.root.join(RUN), fixture.root.join("alias")).unwrap();
        assert!(review(&fixture.root, "alias").is_err());
        assert_eq!(
            serde_json::to_value(list(&fixture.root).unwrap()).unwrap(),
            listed
        );
    }

    #[test]
    fn archive_checks_count_and_compressed_tail_against_receipt() {
        let fixture = Recording::new();
        fixture.save(
            Completed {
                last_seq: fixture.completed.last_seq + 1,
                ..fixture.completed
            },
            fixture.output,
        );
        let checked = fixture.review();
        assert_eq!(checked["receipt_verified"], false);
        assert_eq!(checked["report"]["status"], "matched");
        fixture.save(
            Completed {
                generation: fixture.completed.generation + 1,
                ..fixture.completed
            },
            fixture.output,
        );
        assert_eq!(fixture.review()["receipt_verified"], false);
        fixture.save(fixture.completed, fixture.output);
        let raw = fixture.root.join(RUN).join("raw.ndjson.zst");
        let original = fs::read(&raw).unwrap();
        for (suffix, tail) in [
            (&[0x28_u8, 0xb5][..], "incomplete_frame"),
            (&[1_u8][..], "corrupt_frame"),
        ] {
            let mut bytes = original.clone();
            bytes.extend_from_slice(suffix);
            fs::write(&raw, bytes).unwrap();
            let checked = fixture.review();
            assert_eq!(checked["report"]["raw_tail"], tail);
            assert_eq!(checked["receipt_verified"], false);
            assert_eq!(checked["last_verified_seq"], fixture.completed.last_seq);
        }
        fs::OpenOptions::new()
            .write(true)
            .open(raw)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        assert!(review(&fixture.root, RUN).is_err());
    }
}
