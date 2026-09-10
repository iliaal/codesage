use codesage_graph::{
    IncompleteRiskRanking, assess_risk, assess_risk_batch, build_project_overview,
    build_project_overview_with_top_risk, build_session_snapshot_with_top_risk, full_index,
    persist_session_snapshot, session_start, top_risk_files, top_risk_ranking,
    top_risk_ranking_with_policy,
};
use codesage_protocol::work::{StopReason, WorkControl, WorkStopped};
use codesage_storage::Database;

fn project() -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(root.path().join(name), "pub fn leaf() {}\n").unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    (root, db)
}

#[test]
fn injected_ranking_preserves_overview_and_session_rows() {
    let (root, db) = project();
    let ranking = top_risk_ranking(&db).unwrap();
    assert_eq!(ranking.rows().len(), 3);
    assert_eq!(
        ranking
            .rows()
            .iter()
            .map(|r| r.file.as_str())
            .collect::<Vec<_>>(),
        ["a.rs", "b.rs", "c.rs"]
    );
    let original = build_project_overview(root.path(), &db).unwrap();
    let injected = build_project_overview_with_top_risk(root.path(), &db, &ranking).unwrap();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(injected).unwrap()
    );
    let read = db.read_snapshot().unwrap();
    let assembled =
        build_session_snapshot_with_top_risk(root.path(), &db, "injected", &ranking).unwrap();
    drop(read);
    let ordinary = session_start(root.path(), &db, "ordinary").unwrap();
    assert_eq!(
        serde_json::to_value(assembled.top_risk_files).unwrap(),
        serde_json::to_value(ordinary.top_risk_files).unwrap()
    );
    assert_eq!(assembled.files, ordinary.files);
    assert_eq!(assembled.cycles, ordinary.cycles);
}

#[test]
fn cancelled_ranking_returns_stop_reason_instead_of_fallback_success() {
    let (_root, db) = project();
    let control = WorkControl::new(None);
    let _scope = control.enter();
    control.cancel(StopReason::ClientCancelled);
    let error = top_risk_ranking(&db).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WorkStopped>().unwrap().reason,
        StopReason::ClientCancelled
    );
}

#[test]
fn recovered_batch_failure_produces_complete_ranking() {
    let (root, db) = project();
    // A non-text path breaks bulk decoding but not valid paths' numeric churn queries.
    db.execute_raw_for_tests(
        "INSERT INTO git_files (path, churn_score, total_commits) VALUES
         ('a.rs', 1.0, 4), ('b.rs', 3.0, 4), ('c.rs', 2.0, 4), (X'FF', 4.0, 4)",
    )
    .unwrap();
    let files = ["a.rs", "b.rs", "c.rs"].map(String::from);
    let batch_error = assess_risk_batch(&db, &files).unwrap_err();
    assert!(
        batch_error
            .to_string()
            .contains("bulk churn percentiles for risk batch"),
        "{batch_error:#}"
    );
    let mut expected = files
        .iter()
        .map(|file| {
            let assessment = assess_risk(&db, file).unwrap();
            assert!(assessment.found);
            assert!(!assessment.unscored);
            codesage_protocol::SessionRiskEntry {
                file: assessment.file,
                score: assessment.score,
            }
        })
        .collect::<Vec<_>>();
    expected.sort_by(|a, b| b.score.total_cmp(&a.score));
    assert!(
        expected
            .windows(2)
            .all(|pair| pair[0].score > pair[1].score)
    );
    assert_eq!(
        expected
            .iter()
            .map(|row| row.file.as_str())
            .collect::<Vec<_>>(),
        ["b.rs", "c.rs", "a.rs"]
    );
    let expected = serde_json::to_value(expected).unwrap();
    let ranking = top_risk_ranking(&db).unwrap();
    assert_eq!(serde_json::to_value(ranking.rows()).unwrap(), expected);
    let overview = build_project_overview_with_top_risk(root.path(), &db, &ranking).unwrap();
    assert_eq!(
        serde_json::to_value(overview.top_risk_files).unwrap(),
        expected
    );
    let snapshot =
        build_session_snapshot_with_top_risk(root.path(), &db, "recovered", &ranking).unwrap();
    assert_eq!(
        serde_json::to_value(snapshot.top_risk_files).unwrap(),
        expected
    );
}

#[test]
fn failed_score_components_cannot_construct_complete_ranking() {
    let (_root, db) = project();
    db.execute_raw_for_tests("ALTER TABLE refs RENAME TO broken_refs")
        .unwrap();
    let error = top_risk_ranking(&db).unwrap_err();
    assert!(
        error.downcast_ref::<IncompleteRiskRanking>().is_some(),
        "{error:#}"
    );
    assert!(top_risk_files(&db, 10).is_err());
}

#[test]
fn cancellation_before_persistence_preserves_existing_snapshot() {
    let (root, db) = project();
    session_start(root.path(), &db, "saved").unwrap();
    let path = root.path().join(".codesage/sessions/saved.json");
    let before = std::fs::read(&path).unwrap();
    let ranking = top_risk_ranking_with_policy(&db, false).unwrap();
    let mut replacement =
        build_session_snapshot_with_top_risk(root.path(), &db, "saved", &ranking).unwrap();
    replacement.created_at = 1;
    let control = WorkControl::new(None);
    let _scope = control.enter();
    control.cancel(StopReason::NoConsumers);
    let error = persist_session_snapshot(root.path(), &replacement).unwrap_err();
    assert_eq!(
        error.downcast_ref::<WorkStopped>().unwrap().reason,
        StopReason::NoConsumers
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn cancellation_after_persistence_does_not_remove_committed_snapshot() {
    let (root, db) = project();
    let ranking = top_risk_ranking(&db).unwrap();
    let snapshot =
        build_session_snapshot_with_top_risk(root.path(), &db, "committed", &ranking).unwrap();
    let control = WorkControl::new(None);
    let _scope = control.enter();
    persist_session_snapshot(root.path(), &snapshot).unwrap();
    control.cancel(StopReason::ConnectionClosed);
    let saved: codesage_protocol::SessionSnapshot = serde_json::from_slice(
        &std::fs::read(root.path().join(".codesage/sessions/committed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.session_id, "committed");
    assert_eq!(saved.files, snapshot.files);
}
