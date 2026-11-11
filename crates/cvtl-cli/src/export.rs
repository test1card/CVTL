// FILE: crates/cvtl-cli/src/export.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Publish a verified local run as reproducible derived artifacts
//   SCOPE: Provenance, analysis, HTML and coherent file publication
//   DEPENDS: M-CVTL-ANALYSIS, M-CVTL-STORE
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   export_archive - publish a complete or partial offline run report
// END_MODULE_MAP

mod table;

use std::{
    fs::{self, File, Metadata},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use cvtl_analysis::m8::Options;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{analysis::export_analysis, replay::check_export};
use table::{Tables, CSV, PARQUET};

const REPORT: &str = "report.html";
const SUMMARY: &str = "summary.json";
static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn export_archive(run: &Path, options: Options) -> Result<String, String> {
    let publication_error =
        |error| format!("Ошибка публикации экспорта {} ({error})", run.display());
    let mut staging = Staging::new(run).map_err(publication_error)?;
    let (checked, source, tables) = check_export(
        run,
        |operator_modes| Tables::open(&staging.directory, operator_modes),
        Tables::push,
    )?;
    let rows = tables.finish()?;
    let (completion, analysis) = export_analysis(run, &checked, options)?;
    let status = if completion.status == "verified" {
        "complete"
    } else {
        "partial"
    };
    let raw_identity = if checked.archive.report.raw_verified_records == 0 {
        "unverified"
    } else {
        "matched"
    };
    let conditions = checked.model.as_ref().ok().map(|model| {
        let [stage1, stage2, tf, jacket] = model.initial_k;
        json!({
            "initial_k": {"stage1": stage1, "stage2": stage2, "tf": tf, "jacket": jacket},
            "capacity_stage2_j_per_k": model.capacity_stage2_j_per_k,
            "capacity_tf_j_per_k": model.capacity_tf_j_per_k,
            "stage_link_w_per_k": model.stage_link_w_per_k,
            "fixture_link_w_per_k": model.fixture_link_w_per_k,
            "other_w": model.other_w,
            "wire_reference": model.wire.as_ref().map(|wire| wire.reference.as_str()),
            "group": model.group,
        })
    });
    let csv_sha = file_sha256(&staging.directory.join(CSV)).map_err(publication_error)?;
    let parquet_sha = file_sha256(&staging.directory.join(PARQUET)).map_err(publication_error)?;
    let summary = json!({
        "schema_version": 1,
        "mode": "simulation",
        "status": status,
        "rows": rows,
        "source": source,
        "raw_identity": raw_identity,
        "replay": checked.archive.report,
        "last_verified_seq": checked.archive.last_verified_seq,
        "completion": completion,
        "conditions": conditions,
        "analysis_options": {"start_s": options.start_s, "window_s": options.window_s, "seed": options.seed},
        "analysis": analysis,
        "artifacts": {"ticks.csv": csv_sha, "ticks.parquet": parquet_sha},
    });
    let mut summary_bytes = serde_json::to_vec_pretty(&summary)
        .map_err(|error| format!("Ошибка формирования сводки экспорта ({error})"))?;
    summary_bytes.push(b'\n');
    let digest = format!("{:x}", Sha256::digest(&summary_bytes));
    let derived = format!("derived/{digest}");
    let html = render_html(&summary, &derived);
    let output = json!({"report": REPORT, "derived": derived, "rows": rows, "status": status});
    write_synced(
        File::create_new(staging.directory.join(SUMMARY)).map_err(publication_error)?,
        &summary_bytes,
    )
    .map_err(publication_error)?;
    let expected = [
        (CSV, csv_sha.as_str()),
        (PARQUET, parquet_sha.as_str()),
        (SUMMARY, digest.as_str()),
    ];
    staging
        .publish(run, &digest, &expected, html.as_bytes())
        .map_err(publication_error)?;
    Ok(output.to_string())
}

struct Staging {
    directory: PathBuf,
    report: Option<PathBuf>,
}

impl Staging {
    fn new(run: &Path) -> io::Result<Self> {
        if !fs::symlink_metadata(run)?.is_dir() {
            return Err(io::Error::other("ожидается обычный каталог прогона"));
        }
        if metadata_if_present(&run.join(REPORT))?.is_some_and(|meta| !meta.is_file()) {
            return Err(io::Error::other("report.html должен быть обычным файлом"));
        }
        let derived = run.join("derived");
        match fs::create_dir(&derived) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&derived)?.is_dir() {
                    return Err(io::Error::other("derived должен быть обычным каталогом"));
                }
            }
            Err(error) => return Err(error),
        }
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!(".export-{}-{id}", std::process::id());
            if metadata_if_present(&pending_report_path(run, &name))?.is_some() {
                continue;
            }
            let directory = derived.join(name);
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        report: None,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn publish(
        &mut self,
        run: &Path,
        digest: &str,
        expected: &[(&str, &str)],
        html: &[u8],
    ) -> io::Result<()> {
        let name = self
            .directory
            .file_name()
            .expect("staging directory has a name");
        let pending_report = pending_report_path(run, name);
        let file = File::create_new(&pending_report)?;
        self.report = Some(pending_report.clone());
        write_synced(file, html)?;

        let derived = run.join("derived");
        let target = derived.join(digest);
        if let Some(meta) = metadata_if_present(&target)? {
            verify_generation(&target, &meta, expected)?;
        } else if let Err(error) = fs::rename(&self.directory, &target) {
            // A concurrent export can have installed the same complete set.
            let Some(meta) = metadata_if_present(&target)? else {
                return Err(error);
            };
            verify_generation(&target, &meta, expected)?;
        }
        File::open(&derived)?.sync_all()?;
        fs::rename(pending_report, run.join(REPORT))?;
        self.report = None;
        Ok(())
    }
}

fn pending_report_path(run: &Path, name: impl AsRef<Path>) -> PathBuf {
    run.join(name).with_extension("html.tmp")
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
        if let Some(path) = &self.report {
            let _ = fs::remove_file(path);
        }
    }
}

fn metadata_if_present(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn verify_generation(
    path: &Path,
    metadata: &Metadata,
    expected: &[(&str, &str)],
) -> io::Result<()> {
    let conflict = || io::Error::other(format!("конфликт производного набора {}", path.display()));
    if !metadata.is_dir()
        || fs::read_dir(path)?.try_fold(0, |n, item| item.map(|_| n + 1))? != expected.len()
    {
        return Err(conflict());
    }
    for (name, sha) in expected {
        if file_sha256(&path.join(name)).map_err(|_| conflict())? != *sha {
            return Err(conflict());
        }
    }
    Ok(())
}

fn file_sha256(path: &Path) -> io::Result<String> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(format!(
            "ожидается обычный файл {}",
            path.display()
        )));
    }
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn write_synced(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

fn render_html(summary: &Value, derived: &str) -> String {
    let text = |field: &Value| escape_html(field.as_str().unwrap_or("—"));
    let replay = &summary["replay"];
    let title = text(&replay["run"]);
    let status = if summary["status"] == "complete" {
        "Полный завершённый прогон"
    } else {
        "Проверенный префикс прогона"
    };
    let output = match replay["output_state_at_boundary"].as_str() {
        Some("off_verified") => "OFF подтверждён",
        Some("on_verified") => "ON подтверждён",
        _ => "Состояние выхода неизвестно",
    };
    let completion = match summary["completion"]["status"].as_str() {
        Some("verified") => "Квитанция подтверждена",
        Some("missing") => "Квитанция отсутствует",
        Some("invalid") => "Квитанция непригодна",
        _ => "Квитанция не подтверждает завершение",
    };
    let identity = if summary["raw_identity"] == "matched" {
        "Подтверждена исходной записью"
    } else {
        "Нет проверенной исходной записи"
    };
    let analysis = &summary["analysis"];
    let q2 = &analysis["q2_eff_w"];
    let cooling = if q2.is_null() {
        format!("Не определяется. {}", text(&analysis["reason"]))
    } else {
        format!(
            "{} Вт, интервал 5–95 % от {} до {} Вт",
            q2["nominal"], q2["p05"], q2["p95"]
        )
    };
    let provenance = escape_html(&format!(
        "{:#}",
        json!({
            "Источники": summary["source"],
            "Исходные условия": summary["conditions"],
            "Параметры анализа": summary["analysis_options"],
            "Диагностика реплея": replay["first_diagnostic"],
        })
    ));
    format!(
        "<!doctype html>\n<html lang=\"ru\"><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>CVTL · {title}</title>\n\
         <style>body{{font:1rem/1.5 Inter,Arial,sans-serif;max-width:55rem;margin:2rem auto;padding:0 1rem}}\
         dt{{font-weight:600}}dd{{margin:0 0 .8rem}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}</style>\n\
         <main><h1>Прогон {title}</h1><p>Симуляция. {status}.</p>\n\
         <dl><dt>Кадры raw</dt><dd>{frames} кадров, {records} записей, {tail}</dd>\n\
         <dt>Идентичность raw</dt><dd>{identity}</dd>\n\
         <dt>Семантически проверенный ряд</dt><dd>{rows} тактов до записи {seq}, {replay_status}</dd>\n\
         <dt>Выход на границе</dt><dd>{output}</dd><dt>Завершение</dt><dd>{completion}</dd>\n\
         <dt>Холодопроизводительность Q2_eff</dt><dd>{cooling}</dd></dl>\n\
         <p><a href=\"{derived}/{CSV}\">Таблица CSV</a> · <a href=\"{derived}/{PARQUET}\">Таблица Parquet</a> · \
         <a href=\"{derived}/{SUMMARY}\">Сводка JSON</a></p>\n\
         <details><summary>Условия и происхождение</summary><pre>{provenance}</pre></details></main></html>\n",
        frames = replay["raw_verified_frames"], records = replay["raw_verified_records"],
        tail = text(&replay["raw_tail"]), rows = summary["rows"], seq = summary["last_verified_seq"],
        replay_status = text(&replay["status"]),
    )
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_skips_an_orphaned_report_after_process_restart() {
        let id = NEXT.load(Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("cvtl-export-staging-{pid}-{id}"));
        fs::create_dir(&root).unwrap();
        let orphan = pending_report_path(&root, format!(".export-{pid}-{id}"));
        fs::write(&orphan, b"interrupted publication").unwrap();
        let stage = Staging::new(&root).unwrap();
        assert_ne!(
            pending_report_path(&root, stage.directory.file_name().unwrap()),
            orphan
        );
        drop(stage);
        assert_eq!(fs::read(orphan).unwrap(), b"interrupted publication");
        fs::remove_dir_all(root).unwrap();
    }
}
