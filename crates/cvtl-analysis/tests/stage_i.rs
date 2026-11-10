use cvtl_analysis::stage_i::StageI;

const LEGACY: &str = include_str!("../../../data/old_tests/kde420sa_stage_i_legacy.csv");
const NOMINAL: &str = include_str!("../../../data/machines/kde420sa_stage1_data.csv");

#[test]
fn imports_all_rows_and_maps_historical_temperatures_asymmetrically() {
    let table = StageI::from_legacy_csv(LEGACY).unwrap();
    assert_eq!(table.points().map(|p| p.number), [1, 2, 3, 4, 5, 6, 7, 8]);
    let second = table.points()[1];
    assert_eq!(second.stage2.get(), 6.1);
    assert_eq!(second.stage1.get(), 28.5);
    assert_eq!(second.branches[1].current.unwrap().get(), 0.0);
    assert_eq!(second.branches[1].voltage.get(), 0.0);
    assert_eq!(second.branches[1].calculated_power.unwrap().get(), 0.0);
}

#[test]
fn distinguishes_missing_currents_from_published_and_calculated_power() {
    let table = StageI::from_legacy_csv(LEGACY).unwrap();
    let first = table.points()[0];
    assert_eq!(first.branches.map(|b| b.current), [None, None]);
    assert_eq!(first.branches.map(|b| b.calculated_power), [None, None]);
    assert_eq!(first.branches.map(|b| b.voltage.get()), [0.0, 0.0]);
    assert_eq!(first.published_power.get(), 0.0);
    assert_eq!(first.calculated_power, None);

    let fourth = table.points()[3];
    assert_eq!(fourth.published_power.get(), 5.2);
    assert!((fourth.calculated_power.unwrap().get() - 5.32).abs() < 1e-12);
}

#[test]
fn projects_only_nominal_points_and_retains_inversion_and_uncertainty() {
    let table = StageI::from_legacy_csv(LEGACY).unwrap();
    let seventh = table.points()[6];
    assert_eq!((seventh.stage2.get(), seventh.stage1.get()), (41.3, 39.8));
    assert!(seventh.inverted());
    assert!(!seventh.nominal_anchor());

    let eighth = table.points()[7];
    assert_eq!((eighth.stage2.get(), eighth.stage1.get()), (96.6, 24.5));
    assert!(eighth.stage1_questioned);
    assert!(!eighth.nominal_anchor());

    assert_eq!(
        table.nominal_points().map(|p| p.number).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(table.nominal_csv(), NOMINAL);
    assert_eq!(
        StageI::from_legacy_csv(LEGACY).unwrap().nominal_csv(),
        table.nominal_csv()
    );
}

#[test]
fn rejects_invalid_schema_rows_and_nonfinite_power() {
    for (old, new) in [
        ("T1_K,T2_K", "T2_K,T1_K"),
        ("\n2,", "\n1,"),
        ("0.063", "NaN"),
        ("0.063", "1e308"),
        ("24.5,47.4,true", "24.5,47.4,unknown"),
    ] {
        let invalid = LEGACY.replacen(old, new, 1);
        assert_ne!(invalid, LEGACY);
        assert!(StageI::from_legacy_csv(&invalid).is_err(), "accepted {new}");
    }
    assert!(StageI::from_legacy_csv(LEGACY.lines().next().unwrap()).is_err());
    let extra = format!("{LEGACY}9,0,0,0,0,5,30,0,false\n");
    assert!(StageI::from_legacy_csv(&extra).is_err());
}
