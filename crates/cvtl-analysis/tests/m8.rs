use cvtl_analysis::m8::{analyze, analyze_pair, Interval, Model, Options};

fn model(other_w: Option<f64>) -> Model {
    Model {
        capacity_stage2_j_per_k: 20.0,
        capacity_tf_j_per_k: 5.0,
        stage_link_w_per_k: 0.05,
        fixture_link_w_per_k: 0.5,
        initial_k: [45.0, 15.0, 16.0, 77.0],
        other_w,
        wire: None,
        group: Some("analytical".into()),
    }
}

fn times() -> impl Iterator<Item = (u64, u64)> {
    (0..140).scan(0, |begin, index| {
        let end = *begin
            + if index % 2 == 0 {
                90_000_000
            } else {
                110_000_000
            };
        let interval = (*begin, end);
        *begin = end;
        Some(interval)
    })
}

fn energy_trace(heater_w: f64, cooling_w: f64) -> Vec<Interval> {
    let mut t = [45.0_f64, 15.0, 16.0, 77.0];
    times()
        .map(|(begin_ns, end_ns)| {
            let dt = (end_ns - begin_ns) as f64 * 1e-9;
            let q12 = 0.05 * (t[0] - t[1]);
            let qf2 = 0.5 * (t[2] - t[1]);
            t[0] += dt * (-2.0 - q12) / 10.0;
            t[1] += dt * (q12 + qf2 - cooling_w) / 20.0;
            t[2] += dt * (heater_w - qf2) / 5.0;
            Interval {
                begin_ns,
                end_ns,
                heater_energy_j: [heater_w * dt / 2.0; 2],
                end_k: Some(t),
            }
        })
        .collect()
}

fn options() -> Options {
    Options {
        start_s: Some(1.0),
        ..Options::default()
    }
}

#[test]
fn energy_window_resolves_both_storage_terms_and_is_reproducible() {
    let trace = energy_trace(0.3, 4.0);
    let options = options();
    let model = model(Some(0.0));
    let report = analyze(&model, &trace, options);
    let q = report.q2_eff_w.unwrap();
    assert!((q.nominal - 4.0).abs() < 1e-10);
    assert!(q.p05 <= 4.0 && 4.0 <= q.p95);
    assert!(q.std_dev > 0.0);
    assert_eq!(report, analyze(&model, &trace, options));
    let window = report.window.unwrap();
    assert_eq!(window.start_s, 1.0);
    assert_eq!(window.end_s, 13.0);
    assert!((window.mean_heater_w - 0.3).abs() < 1e-12);
    assert!(window.derivative_stage2_k_per_s.nominal < 0.0);
    assert!(window.derivative_tf_k_per_s.std_dev > 0.0);
}

#[test]
fn quadratic_fit_on_irregular_model_times_has_exact_center_slope() {
    let trace = times()
        .map(|(begin_ns, end_ns)| {
            let t = end_ns as f64 * 1e-9 - 7.0;
            Interval {
                begin_ns,
                end_ns,
                heater_energy_j: [0.02, 0.03],
                end_k: Some([
                    45.0,
                    18.0 - 0.2 * t + 0.03 * t * t,
                    16.0 + 0.1 * t + 0.02 * t * t,
                    77.0,
                ]),
            }
        })
        .collect::<Vec<_>>();
    let report = analyze(&model(Some(0.0)), &trace, options());
    let window = report.window.unwrap();
    assert!((window.derivative_stage2_k_per_s.nominal + 0.2).abs() < 1e-12);
    assert!((window.derivative_tf_k_per_s.nominal - 0.1).abs() < 1e-12);
    assert!((window.fixture_compensation_delta_w - 0.5).abs() < 1e-10);
}

#[test]
fn missing_or_discontinuous_observations_and_unknown_heat_are_undefined() {
    let trace = energy_trace(0.3, 4.0);
    assert_eq!(
        analyze(&model(None), &trace, options()).status,
        "не определяется"
    );
    let known = model(Some(0.0));
    let mut broken = trace.clone();
    broken[60].end_k = None;
    assert!(analyze(&known, &broken, options()).q2_eff_w.is_none());
    let mut gapped = trace.clone();
    gapped.remove(60);
    assert!(analyze(&known, &gapped, options()).q2_eff_w.is_none());
    assert!(analyze(&known, &trace[..5], options()).q2_eff_w.is_none());
    assert!(analyze(
        &known,
        &trace,
        Options {
            window_s: 9.0,
            ..options()
        }
    )
    .q2_eff_w
    .is_none());
}

#[test]
fn paired_excitation_identifies_fixture_capacity_and_common_cooling() {
    let low = energy_trace(0.3, 3.0);
    let high = energy_trace(2.8, 3.0);
    let model = model(Some(0.0));
    let report = analyze_pair(&model, &low, &model, &high, options());
    let capacity = report.identified_cf_j_per_k.unwrap();
    let q = report.q2_eff_w.unwrap();
    assert!((capacity.nominal - 5.0).abs() < 1e-9);
    assert!(capacity.p05 <= 5.0 && 5.0 <= capacity.p95);
    assert!((q.nominal - 3.0).abs() < 1e-9);
    assert!(q.p05 <= 3.0 && 3.0 <= q.p95);
    assert!(report.paired_window.is_some());
    assert_eq!(report, analyze_pair(&model, &low, &model, &high, options()));
    assert!(analyze_pair(&model, &low, &model, &low, options())
        .q2_eff_w
        .is_none());
    let mut different = model.clone();
    different.group = Some("different".into());
    assert!(analyze_pair(&model, &low, &different, &high, options())
        .q2_eff_w
        .is_none());
}
