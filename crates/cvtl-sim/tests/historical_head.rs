use cvtl_analysis::stage_i::StageI;
use cvtl_core::units::Kelvin;
use cvtl_sim::historical_head::{report, HistoricalHead, HistoricalOrigin};

fn kelvin(value: f64) -> Kelvin {
    Kelvin::new(value).unwrap()
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() <= 1e-10, "{actual} vs {expected}");
}

#[test]
fn historical_anchors_are_returned_exactly() {
    let model = HistoricalHead::new_test_only().unwrap();
    let anchors = [
        (4.95, 0.0, 28.45),
        (6.1, 0.63, 28.5),
        (7.14, 1.31, 28.6),
        (11.2, 5.2, 30.5),
        (16.4, 12.0, 39.0),
        (23.4, 22.0, 43.8),
    ];
    for (index, (stage2, power, stage1)) in anchors.into_iter().enumerate() {
        let point = model.at(kelvin(stage2)).unwrap();
        assert_eq!(point.stage2_k, stage2);
        assert_eq!(point.p_net_w, power);
        assert_eq!(point.stage1_k, stage1);
        assert_eq!(point.origin, HistoricalOrigin::HistoricalAnchor);
        assert_eq!(point.source_row, Some(index as u8 + 1));
    }
}

#[test]
fn canonical_source_is_the_existing_stage_i_nominal_projection() {
    let stage_i = StageI::from_legacy_csv(include_str!(
        "../../../data/old_tests/kde420sa_stage_i_legacy.csv"
    ))
    .unwrap();
    assert_eq!(
        stage_i.nominal_csv(),
        include_str!("../../../data/machines/kde420sa_stage1_data.csv")
    );
}

#[test]
fn analytical_interpolation_and_continuation() {
    let model = HistoricalHead::new_test_only().unwrap();
    for (stage2, power, stage1, origin) in [
        (6.62, 0.97, 28.55, HistoricalOrigin::Interpolated),
        (19.9, 17.0, 41.4, HistoricalOrigin::Interpolated),
        (
            161.7,
            219.571_428_571_428_57,
            171.9,
            HistoricalOrigin::Synthetic,
        ),
        (
            300.0,
            417.142_857_142_857_17,
            300.0,
            HistoricalOrigin::Synthetic,
        ),
    ] {
        let point = model.at(kelvin(stage2)).unwrap();
        assert_eq!(point.stage2_k, stage2);
        close(point.p_net_w, power);
        close(point.stage1_k, stage1);
        assert_eq!(point.origin, origin);
        assert_eq!(point.source_row, None);
    }
}

#[test]
fn closed_range_and_continuous_join() {
    let model = HistoricalHead::new_test_only().unwrap();
    assert!(model.at(kelvin(4.95_f64.next_down())).is_err());
    assert!(model.at(kelvin(300.0_f64.next_up())).is_err());
    let join = 23.4_f64;
    for (temperature, origin) in [
        (join.next_down(), HistoricalOrigin::Interpolated),
        (join.next_up(), HistoricalOrigin::Synthetic),
    ] {
        let point = model.at(kelvin(temperature)).unwrap();
        close(point.stage1_k, 43.8);
        close(point.p_net_w, 22.0);
        assert_eq!(point.origin, origin);
    }
}

#[test]
fn sampled_characteristic_is_finite_monotonic_and_not_inverted() {
    let model = HistoricalHead::new_test_only().unwrap();
    let mut previous = model.at(kelvin(4.95)).unwrap();
    for index in 0..=1000 {
        let temperature = 4.95 + (300.0 - 4.95) * f64::from(index) / 1000.0;
        let point = model.at(kelvin(temperature)).unwrap();
        assert!(point.stage1_k.is_finite() && point.p_net_w.is_finite());
        assert!(point.stage1_k >= point.stage2_k);
        assert!(point.stage1_k >= previous.stage1_k);
        assert!(point.p_net_w >= previous.p_net_w);
        previous = point;
    }
}

#[test]
fn report_exposes_origin_conditions_and_continuation() {
    let report = report().unwrap();
    assert!(report.synthetic && report.test_only);
    assert!(!report.liquid_nitrogen && !report.radiation_shield && !report.parasitics_removed);
    assert_eq!(report.source_path, "data/machines/kde420sa_stage1_data.csv");
    assert_eq!(
        report.source_sha256,
        "b69a6969815184f8b88e384810d14e5dd05cf7c7df40523e668d918250906691"
    );
    assert_eq!(report.nominal_rows, [1, 2, 3, 4, 5, 6]);
    assert_eq!(report.continuation_from_k, 23.4);
    assert_eq!(report.continuation_to_k, 300.0);
    close(report.power_slope_w_per_k, 10.0 / 7.0);
    close(report.stage_gap_at_start_k, 20.4);
    assert_eq!(report.points.len(), 15);
    for (index, point) in report.points[..11].iter().enumerate() {
        if index % 2 == 0 {
            assert_eq!(point.origin, HistoricalOrigin::HistoricalAnchor);
            assert_eq!(point.source_row, Some(index as u8 / 2 + 1));
        } else {
            assert_eq!(point.origin, HistoricalOrigin::Interpolated);
            assert_eq!(point.source_row, None);
            close(
                point.stage2_k,
                (report.points[index - 1].stage2_k + report.points[index + 1].stage2_k) / 2.0,
            );
        }
    }
    for (point, temperature) in report.points[11..].iter().zip([50.0, 100.0, 200.0, 300.0]) {
        assert_eq!(point.origin, HistoricalOrigin::Synthetic);
        assert_eq!(point.stage2_k, temperature);
        assert_eq!(point.source_row, None);
    }
    assert!(report
        .points
        .windows(2)
        .all(|pair| pair[0].stage2_k < pair[1].stage2_k));
}
