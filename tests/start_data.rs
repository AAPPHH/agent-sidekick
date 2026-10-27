use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_sidekick::backends::{Backend, BackendKind, CodexAuth, Cost, OpencodeProvider};
use agent_sidekick::data::{pseudonym, write_events, DataDir, ModelRow, ProtectedRow};
use agent_sidekick::dispatch::{technical_sperre, Identity};
use agent_sidekick::trust::{Dimension, Event, Outcome, Scope};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const ADDED_AT: i64 = 1_793_102_400_000;
const QWEN: &str = "Qwen3-Coder-Next";
const KIC_MODEL: &str = "openai-gpt-oss-120b";

fn backend(kind: BackendKind) -> Backend {
    Backend {
        name: "b".into(),
        description: "t".into(),
        cost: Cost::Free,
        is_default: false,
        max_concurrent: None,
        model: None,
        models: None,
        env: BTreeMap::new(),
        pass_env: Vec::new(),
        implicit: false,
        kind,
        foreign_secret_env: Vec::new(),
    }
}

fn claude() -> Backend {
    backend(BackendKind::ClaudeOauth { config_dir: PathBuf::from("/cfg"), permissions: Default::default() })
}

fn dgx() -> Backend {
    backend(BackendKind::OpenCode {
        provider: OpencodeProvider::OpenaiCompatible,
        base_url: Some("http://dgx.mv.intern.hs-duesseldorf.de:18080/v1".into()),
        api_key_env: None,
        harness_path: None,
        read_only: false,
        request_timeout_s: None,
        model_options: Default::default(),
        base_dir: PathBuf::from("/x"),
    })
}

fn kiconnect_codex() -> Backend {
    backend(BackendKind::Codex {
        auth: CodexAuth::ApiKey,
        base_url: Some("https://chat.kiconnect.nrw/api/v1".into()),
        api_key_env: None,
        harness_path: None,
        read_only: false,
        windows_sandbox: Default::default(),
        base_dir: PathBuf::from("/x"),
    })
}

fn row(b: &Backend, model: &str, cost: &str, max_steps: i64, context_tokens: Option<i64>, budget: i64) -> ModelRow {
    ModelRow {
        key: Identity::default().key(b, Some(model)),
        context_tokens,
        cost: cost.into(),
        max_steps: Some(max_steps),
        tool_output_lines: Some(200),
        context_budget_tokens: Some(budget),
        sperre: technical_sperre(b),
        added_at: ADDED_AT,
        added_by: "aapphh".into(),
    }
}

fn models() -> Vec<ModelRow> {
    vec![
        row(&dgx(), QWEN, "shared-dgx", 80, None, 64_000),
        row(&kiconnect_codex(), KIC_MODEL, "free", 80, None, 64_000),
        row(&claude(), "claude-sonnet-5-5", "quota", 100, Some(200_000), 150_000),
        row(&claude(), "claude-opus-5-5", "quota", 100, Some(200_000), 150_000),
    ]
}

fn events(qwen_id: &str) -> Vec<Event> {
    let user = pseudonym(b"start-data", "aapphh-user");
    let machine = pseudonym(b"start-data", "aapphh-machine");
    let make = |dimension, outcome, ts| {
        let mut e = Event::new(qwen_id, dimension, Scope::Global, outcome, ADDED_AT + ts);
        e.user_id = user.clone();
        e.machine_id = machine.clone();
        e
    };
    let mut incident = make(Dimension::WriteCode, Outcome::Blocked, 1_000);
    incident.steps = Some(80);
    incident.duration_s = Some(142.0);
    incident.block_reason = Some("Validierung decided_by entfernt".into());
    incident.summary = Some("Qwen3-Coder-Next entfernte decided_by aus der Validierung; Diff verworfen.".into());
    incident.conclusion = Some("Kein Schreibrecht an Validierungscode; Entscheidungen vorher im Task festlegen.".into());
    let mut search = make(Dimension::Search, Outcome::Seed, 2_000);
    search.seed_value = Some(0.55);
    let mut inventory = make(Dimension::Inventory, Outcome::Seed, 3_000);
    inventory.seed_value = Some(0.35);
    vec![incident, search, inventory]
}

#[test]
#[ignore]
fn generate_start_data() {
    let data = PathBuf::from(ROOT).join("data");
    ModelRow::write_all(&data.join("models.parquet"), &models()).unwrap();
    ProtectedRow::write_all(&data.join("protected.parquet"), &ProtectedRow::defaults()).unwrap();
    let qwen_id = models()[0].model_id();
    let session = uuid::Uuid::new_v4();
    write_events(&data.join(format!("experiences/aapphh/{session}.parquet")), &events(&qwen_id)).unwrap();
    std::fs::write(data.join("peers.toml"), "aapphh = \"AAPPHH\"\n").unwrap();
}

#[test]
fn committed_start_data_matches_dispatch_identity() {
    let data = DataDir::new(PathBuf::from(ROOT).join("data"));
    let ids: Vec<String> = data.models().unwrap().iter().map(ModelRow::model_id).collect();
    let expected: Vec<String> = models().iter().map(ModelRow::model_id).collect();
    assert_eq!(ids, expected);
    assert_eq!(data.protected().unwrap(), ProtectedRow::defaults());
    let now = chrono::Utc::now().timestamp_millis();
    let sb = data.steckbrief(&expected[0], &Scope::Global, None, now).unwrap();
    let trust = |d| sb.rows.iter().find(|r| r.dimension == d).unwrap().trust;
    assert!((trust(Dimension::Search) - 0.55).abs() < 1e-9);
    assert!((trust(Dimension::Inventory) - 0.35).abs() < 1e-9);
    assert!((trust(Dimension::WriteCode) - 0.10).abs() < 1e-9);
    assert!(!sb.write_right_code && !sb.decisions_right);
    assert!(data.steckbrief(&expected[3], &Scope::Global, None, now).unwrap().gold);
    assert!(!data.steckbrief(&expected[2], &Scope::Global, None, now).unwrap().gold);
}
