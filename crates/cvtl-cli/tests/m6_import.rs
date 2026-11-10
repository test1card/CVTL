use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn import(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
        .arg("import-stage-i")
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn cli_preserves_provenance_values_and_source_bytes() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/old_tests/kde420sa_stage_i_legacy.csv");
    let before = fs::read(&path).unwrap();
    let hash = format!("{:x}", Sha256::digest(&before));

    let first = import(&path);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(output["source_sha256"], hash);
    assert_eq!(output["format_reference"], "TZ.md §2.3");
    assert_eq!(
        output["reference_conditions"],
        json!({"liquid_nitrogen": false, "radiation_shield": false})
    );
    assert_eq!(output["nominal_anchor_rows"], json!([1, 2, 3, 4, 5, 6]));
    let points = output["points"].as_array().unwrap();
    assert_eq!(points.len(), 8);
    assert_eq!(points[0]["published_power_w"], 0.0);
    assert_eq!(points[0]["calculated_power_w"], Value::Null);
    assert_eq!(points[0]["branches"][0]["current_a"], Value::Null);
    assert_eq!(points[0]["branches"][0]["voltage_v"], 0.0);
    assert_eq!(points[1]["branches"][1]["current_a"], 0.0);
    assert_eq!(points[1]["stage2_k"], 6.1);
    assert_eq!(points[1]["stage1_k"], 28.5);
    assert!(points[1].get("T1").is_none() && points[1].get("T2").is_none());
    assert_eq!(points[3]["published_power_w"], 5.2);
    assert!((points[3]["calculated_power_w"].as_f64().unwrap() - 5.32).abs() < 1e-12);
    assert_eq!(points[6]["inverted"], true);
    assert_eq!(points[6]["stage2_k"], 41.3);
    assert_eq!(points[6]["stage1_k"], 39.8);
    assert_eq!(points[7]["stage1_questioned"], true);
    assert_eq!(points[7]["stage1_k"], 24.5);
    assert_eq!(points[7]["nominal_anchor"], false);

    let second = import(&path);
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(before, fs::read(&path).unwrap());
}

#[test]
fn cli_rejects_unknown_schema_without_writing() {
    let path = std::env::temp_dir().join(format!("cvtl-import-invalid-{}.csv", std::process::id()));
    let input = b"stage1,stage2,power\n42,10,1\n";
    fs::write(&path, input).unwrap();
    let output = import(&path);
    let after = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("неверные заголовки"));
    assert_eq!(after, input);
}
