// SPDX-License-Identifier: Apache-2.0

fn assert_each_repeated_report_changes_nothing(mut trees: Trees, label: &str) {
    let ids: Vec<NodeId> = trees.local().iter().map(|(id, _)| id.clone()).collect();
    for id in ids {
        let node = trees.local().get(&id).unwrap().clone();
        let before = format!("{:?}", trees.state());
        trees.set_local_changed(id.clone(), node);
        assert_eq!(format!("{:?}", trees.state()), before, "{label}: {id}");
    }
}

#[test]
fn a_scan_that_reports_a_file_as_the_engine_holds_it_changes_no_state() {
    let (_, trees, _) = edited_at_5_then_lost();
    assert_each_repeated_report_changes_nothing(trees, "edited then lost");
    let (_, trees, _) = an_edit_in_flight_of_a_file_another_device_moved();
    assert_each_repeated_report_changes_nothing(trees, "edit in flight, moved elsewhere");
    let (trees, _, _) = resent_where_another_device_renamed_it(Some(2));
    assert_each_repeated_report_changes_nothing(trees, "resent edit, renamed elsewhere");
    let (trees, _, _) = resent_where_another_device_renamed_it(None);
    assert_each_repeated_report_changes_nothing(trees, "resent, renamed elsewhere");
}

#[test]
fn a_scan_that_finds_a_file_back_at_its_place_ends_its_record_in_an_ignored_folder() {
    let mut trees = trees(&[(
        "a",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    trees.ignore_local(&"a".into());
    assert!(trees.recorded_local(&"a".into()));
    let node = trees.local().get(&"a".into()).unwrap().clone();
    trees.set_local_changed("a".into(), node);
    assert!(!trees.recorded_local(&"a".into()));
}
