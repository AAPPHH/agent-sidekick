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
    assert_eq!((v.model_id.as_str(), v.dimension, v.steps, v.duration_s), ("mid", Dimension::WriteCode, Some(7), Some(12.5)));
    assert_eq!(v.scope, Scope::Repo("r".into()));
    assert!(v.review_s.unwrap() >= 5.0 && v.review_s.unwrap() < 60.0);
}

#[test]
fn verdict_rejects_unknown_double_blocked_and_wrong_outcome() {
    let (_d, s) = store();
    assert!(s.record_verdict("nope", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("unknown job_id"));
    s.append(job_event("j2", Outcome::Note, 1000)).unwrap();
    assert!(s.record_verdict("j2", Outcome::Note, None, Visibility::Private).is_err());
    s.record_verdict("j2", Outcome::Rejected, None, Visibility::Private).unwrap();
    assert!(s.record_verdict("j2", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("already has a verdict"));
    s.append(job_event("j3", Outcome::Blocked, 1000)).unwrap();
    assert!(s.record_verdict("j3", Outcome::Accepted, None, Visibility::Private).unwrap_err().to_string().contains("blocked"));
}
