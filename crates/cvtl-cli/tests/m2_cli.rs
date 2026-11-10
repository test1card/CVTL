use std::{path::Path, process::Command};

fn invoke(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cvtl-cli"))
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn copper_benchmark_reports_load_at_both_anchors() {
    let output = invoke(&["wire", "data/scenarios/m2-wire-copper.yaml"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("stage2: 0.048881 Вт"), "{text}");
    assert!(text.contains("jacket: -0.048881 Вт"), "{text}");
}

#[test]
fn advisor_reports_the_nominal_result_and_missing_heater_limits() {
    let output = invoke(&["heater-advisor", "data/scenarios/m2-heater-candidates.yaml"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("32.400"), "{text}");
    assert!(text.contains("64.800"), "{text}");
    assert!(text.contains("предел нагревателя не задан"), "{text}");
}

#[test]
fn missing_input_is_an_error() {
    let output = invoke(&["wire", "data/scenarios/missing-m2-scenario.yaml"]);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
}

#[test]
fn wire_reports_joule_heat_and_sums_flows_at_an_intercept() {
    let output = invoke(&["wire", "crates/cvtl-cli/tests/fixtures/wire-test-only.yaml"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Джоулево тепло 0.000250 Вт"), "{text}");
    assert!(
        text.contains("stage2: 0.024565 Вт, сценарный диапазон"),
        "{text}"
    );
    let jacket = text
        .lines()
        .find(|line| line.starts_with("  jacket:"))
        .unwrap();
    // The 80→77 K strand contributes 0.000192 W, and the 77→4.2 K strand
    // removes 0.024315 W after its warm half of the Joule heat is included.
    assert!(
        jacket.contains("-0.024123 Вт, сценарный диапазон"),
        "{jacket}"
    );
}
