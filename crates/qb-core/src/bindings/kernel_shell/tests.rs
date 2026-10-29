use super::super::permit_fixture::scripted_hour;
use super::*;
use chrono::Duration as Delta;

#[test]
fn native_scripted_hour_records_every_entry_with_independent_expected_outcomes() {
    let directory = super::super::tests::fixture_dir();
    let table = BindingTable::new(directory.path(), RoutingConfig::default()).unwrap();
    let records = Arc::new(Mutex::new(Vec::new()));
    let captured = records.clone();
    table.set_permit_recorder(Arc::new(move |inputs, actual| {
        let mut rows = captured.lock().unwrap();
        rows.push((inputs.clone(), actual.clone()));
        Some(rows.len() as u64)
    }));
    scripted_hour(&table);
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 63);
    assert_eq!(
        records.last().unwrap().0.now - records[0].0.now,
        Delta::hours(1)
    );
    assert_eq!(
        records
            .iter()
            .filter(|(_, d)| matches!(d, kernel::PermitDecision::Allowed(_)))
            .count(),
        4
    );
    assert_eq!(
        records
            .iter()
            .filter(|(_, d)| matches!(d, kernel::PermitDecision::Denied(_)))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|(_, d)| matches!(d, kernel::PermitDecision::NotRequired))
            .count(),
        58
    );
    for (inputs, actual) in records.iter() {
        assert_eq!(&kernel::decide_move(inputs), actual);
    }
}
