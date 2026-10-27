use agent_sidekick::sync::DataStore;
use agent_sidekick::trust::{Dimension, Event, Outcome, Scope, Visibility};

fn store() -> (tempfile::TempDir, DataStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::open(dir.path(), "x/y", None).unwrap();
    (dir, store)
}

fn job_event(job: &str, outcome: Outcome, age_ms: i64) -> Event {
    let mut e = Event::new("mid", Dimension::WriteCode, Scope::Repo("r".into()), outcome, chrono::Utc::now().timestamp_millis() - age_ms);
    e.job_id = Some(job.into());
    e.summary = Some("status: completed".into());
    e.steps = Some(7);
    e.duration_s = Some(12.5);
    e
}

fn own(store: &DataStore) -> Vec<Event> {
    agent_sidekick::data::read_events(&store.local_root.join(format!("experiences/local/{}", std::fs::read_dir(store.local_root.join("experiences/local")).unwrap().next().unwrap().unwrap().file_name().to_string_lossy()))).unwrap()
}

#[test]
fn verdict_copies_the_job_event_and_measures_review_time() {
    let (_d, s) = store();
    s.append(job_event("j1", Outcome::Note, 5000)).unwrap();
    s.record_verdict("j1", Outcome::Accepted, Some("fine".into()), Visibility::Private).unwrap();
    let events = own(&s);
    let v = events.iter().find(|e| e.outcome == Outcome::Accepted).unwrap();
    assert_eq!((v.model_id.as_str(), v.dimension, v.steps, v.duration_s), ("mid", Dimension::WriteCode, None, None));
    assert_eq!(v.scope, Scope::Repo("r".into()));
    assert!(v.review_s.unwrap() >= 5.0 && v.review_s.unwrap() < 60.0);
}

#[test]
fn verdict_rejects_unknown_double_blocked_and_wrong_outcome() {
    let (_d, s) = store();
    assert!(s.record_verdict("nope", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("unknown job_id"));
    s.append(job_event("j2", Outcome::Note, 1000)).unwrap();
    assert!(s.record_verdict("j2", Outcome::Note, None, Visibility::Private).is_err());
    s.record_verdict("j2", Outcome::Rejected, Some("Datei nennen".into()), Visibility::Private).unwrap();
    assert!(s.record_verdict("j2", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("already has a verdict"));
    s.append(job_event("j3", Outcome::Blocked, 1000)).unwrap();
    assert!(s.record_verdict("j3", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("blocked"));
}

#[test]
fn only_completed_jobs_get_a_verdict() {
    let (_d, s) = store();
    for status in ["nochange", "nopatch", "aborted", "failed"] {
        let mut e = job_event(&format!("j-{status}"), Outcome::Note, 1000);
        e.summary = Some(format!("status: {status}"));
        s.append(e).unwrap();
        let err = s.record_verdict(&format!("j-{status}"), Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string();
        assert!(err.contains("only completed jobs"), "{err}");
    }
    s.append(job_event("j-aborted-event", Outcome::Aborted, 1000)).unwrap();
    assert!(s.record_verdict("j-aborted-event", Outcome::Accepted, None, Visibility::Private).is_err());
    let mut legacy = job_event("j-legacy", Outcome::Note, 1000);
    legacy.summary = None;
    s.append(legacy).unwrap();
    assert!(s.record_verdict("j-legacy", Outcome::Accepted, None, Visibility::Private).is_err());
    assert!(own(&s).iter().all(|e| !matches!(e.outcome, Outcome::Accepted | Outcome::Rejected)));
}

#[test]
fn concurrent_verdicts_for_one_job_write_exactly_one() {
    let (_d, s) = store();
    s.append(job_event("jc", Outcome::Note, 1000)).unwrap();
    let ok = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| s.record_verdict("jc", Outcome::Accepted, None, Visibility::Private).is_ok())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).filter(|ok| *ok).count()
    });
    assert_eq!(ok, 1);
    assert_eq!(own(&s).iter().filter(|e| e.outcome == Outcome::Accepted).count(), 1);
}

#[test]
fn cost_balance_counts_a_job_runtime_once() {
    let (_d, s) = store();
    s.append(job_event("jd", Outcome::Note, 5000)).unwrap();
    s.record_verdict("jd", Outcome::Accepted, None, Visibility::Private).unwrap();
    let events = own(&s);
    assert_eq!(events.iter().filter_map(|e| e.duration_s).sum::<f64>(), 12.5);
    let mut legacy = events.iter().find(|e| e.outcome == Outcome::Accepted).unwrap().clone();
    legacy.event_id = "legacy".into();
    legacy.duration_s = Some(12.5);
    let mut all = events;
    all.push(legacy);
    let dir = tempfile::tempdir().unwrap();
    let data = agent_sidekick::data::DataDir::new(dir.path());
    agent_sidekick::data::ModelRow::write_all(&dir.path().join("models.parquet"), &[agent_sidekick::data::ModelRow {
        key: agent_sidekick::trust::ModelKey::new("p", "m", "m", "none", "h"),
        context_tokens: None,
        cost: "free".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: agent_sidekick::trust::Sperre::NurDiff,
        added_at: 0,
        added_by: "t".into(),
    }]).unwrap();
    let id = agent_sidekick::trust::ModelKey::new("p", "m", "m", "none", "h").model_id();
    for e in &mut all {
        e.model_id = id.clone();
    }
    agent_sidekick::data::write_events(&dir.path().join("experiences/a/s.parquet"), &all).unwrap();
    let sb = data.steckbrief(&id, &Scope::Global, None, chrono::Utc::now().timestamp_millis()).unwrap();
    assert_eq!(sb.duration_s, 12.5);
}

#[test]
fn rejection_needs_a_formulation_rule() {
    let (_d, s) = store();
    s.append(job_event("jr", Outcome::Note, 1000)).unwrap();
    for c in [None, Some(String::new()), Some("  ".to_string())] {
        let err = s.record_verdict("jr", Outcome::Rejected, c, Visibility::Private).unwrap_err().to_string();
        assert!(err.contains("non-empty `conclusion`"), "{err}");
    }
    assert!(own(&s).iter().all(|e| e.outcome != Outcome::Rejected));
    s.record_verdict("jr", Outcome::Rejected, Some("Zielmodul nennen".into()), Visibility::Private).unwrap();
}
