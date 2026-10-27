mod support;

use std::path::Path;
use std::process::Command;

use agent_sidekick::data::ModelRow;
use agent_sidekick::trust::{ModelKey, Sperre};
use serde_json::json;
use support::{text_of, Server};

fn sh(dir: &Path, args: &[&str]) {
    assert!(Command::new("git").arg("-C").arg(dir).args(args).status().unwrap().success());
}

fn fixture(register: bool) -> (tempfile::TempDir, Server, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let checkout = dir.path().join("data/checkout");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    sh(&checkout, &["init", "-q", "-b", "main"]);
    let script = dir.path().join("fakecmd.sh");
    std::fs::write(&script, "#!/bin/sh\necho RAN\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bad = dir.path().join("bad.sh");
    std::fs::write(&bad, "#!/bin/sh\necho BAD\nexit 1\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        format!("[backends.cmd]\ntype = \"command\"\ndescription = \"t\"\ndefault = true\ncommand = ['{}', '{{prompt}}']\n[backends.bad]\ntype = \"command\"\ndescription = \"t\"\ncommand = ['{}', '{{prompt}}']\n", script.display(), bad.display()),
    )
    .unwrap();
    if register {
        let row = ModelRow {
            key: ModelKey::new("fakecmd.sh", "default", "default", "none", "command"),
            context_tokens: None,
            cost: "free".into(),
            max_steps: None,
            tool_output_lines: None,
            context_budget_tokens: None,
            sperre: Sperre::NurDiff,
            added_at: 0,
            added_by: "t".into(),
        };
        ModelRow::write_all(&checkout.join("models.parquet"), &[row]).unwrap();
    }
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "i"]);
    let data = dir.path().join("data");
    let mut server = Server::start(&cfg, &[("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())]);
    server.initialize();
    (dir, server, repo)
}

#[test]
fn registered_backend_needs_dimension_and_runs_as_dispatch() {
    let (_d, mut s, repo) = fixture(true);
    let cwd = repo.to_string_lossy().to_string();
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd}));
    assert_eq!(res["isError"], json!(true));
    assert!(text_of(&res).contains("needs `dimension`"), "{}", text_of(&res));
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd, "dimension": "search"}));
    let text = text_of(&res);
    assert!(text.contains("status: NoChange") && text.contains("model_id:"), "{text}");
    assert!(text.contains("event_id:"), "{text}");
}

#[test]
fn unregistered_backend_keeps_plain_run() {
    let (_d, mut s, repo) = fixture(false);
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": repo.to_string_lossy()}));
    assert!(res["isError"] != json!(true), "{res}");
    assert!(text_of(&res).contains("RAN"), "{}", text_of(&res));
}

#[test]
fn registered_fallback_goes_through_dispatch_or_is_rejected() {
    let (_d, mut s, repo) = fixture(true);
    let cwd = repo.to_string_lossy().to_string();
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd, "backend": "bad", "fallback": "cmd"}));
    assert_eq!(res["isError"], json!(true));
    assert!(text_of(&res).contains("needs `dimension`"), "{}", text_of(&res));
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd, "backend": "bad", "fallback": "cmd", "dimension": "search"}));
    let text = text_of(&res);
    assert!(text.contains("status: NoChange") && text.contains("model_id:") && text.contains("event_id:"), "{text}");
    assert!(!text.contains("RAN\n") || text.contains("--- output tail ---"), "{text}");
    assert!(repo.join("a.txt").exists());
}

#[test]
fn unregistered_fallback_still_runs_plain() {
    let (_d, mut s, repo) = fixture(false);
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": repo.to_string_lossy(), "backend": "bad", "fallback": "cmd"}));
    assert!(res["isError"] != json!(true), "{res}");
    assert!(text_of(&res).contains("RAN"), "{}", text_of(&res));
}

#[test]
fn unreadable_data_store_fails_closed() {
    let (d, mut s, repo) = fixture(true);
    drop(s);
    let checkout = d.path().join("data/checkout");
    std::fs::remove_dir_all(&checkout).unwrap();
    std::fs::write(&checkout, "not a directory").unwrap();
    let cfg = d.path().join("cfg");
    let data = d.path().join("data");
    let mut s = Server::start(&cfg, &[("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())]);
    s.initialize();
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": repo.to_string_lossy()}));
    assert_eq!(res["isError"], json!(true), "{res}");
    assert!(text_of(&res).contains("data store"), "{}", text_of(&res));
    assert!(!text_of(&res).contains("RAN"));
}

#[test]
fn missing_checkout_is_created_and_the_registry_applies() {
    let (d, mut s, repo) = fixture(true);
    drop(s);
    std::fs::remove_dir_all(d.path().join("data/checkout")).unwrap();
    let cfg = d.path().join("cfg");
    let data = d.path().join("data");
    let mut s = Server::start(&cfg, &[("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())]);
    s.initialize();
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": repo.to_string_lossy()}));
    assert!(res["isError"] != json!(true), "{res}");
    assert!(d.path().join("data/checkout/.git").exists());
}

#[test]
fn unregistered_model_on_a_backend_with_registered_models_is_rejected() {
    let (d, mut s, repo) = fixture(true);
    drop(s);
    let checkout = d.path().join("data/checkout");
    let row = ModelRow {
        key: ModelKey::new("anthropic", "claude-sonnet-5-5", "claude-sonnet-5-5", "none", "claude-code"),
        context_tokens: None,
        cost: "quota".into(),
        max_steps: None,
        tool_output_lines: None,
        context_budget_tokens: None,
        sperre: Sperre::Technisch,
        added_at: 0,
        added_by: "t".into(),
    };
    ModelRow::write_all(&checkout.join("models.parquet"), &[row]).unwrap();
    let cfg = d.path().join("cfg");
    let mut text = std::fs::read_to_string(cfg.join("config.toml")).unwrap();
    text.push_str("[backends.cl]\ntype = \"claude-oauth\"\ndescription = \"t\"\n");
    std::fs::write(cfg.join("config.toml"), text).unwrap();
    let data = d.path().join("data");
    let mut s = Server::start(&cfg, &[("AGENT_SIDEKICK_DATA_DIR", data.to_str().unwrap())]);
    s.initialize();
    let cwd = repo.to_string_lossy().to_string();
    for model in ["claude-haiku-5-5", "claude-sonnet-5-5-evil"] {
        let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd, "backend": "cl", "model": model, "dimension": "search"}));
        assert_eq!(res["isError"], json!(true), "{res}");
        assert!(text_of(&res).contains("not one of them"), "{}", text_of(&res));
    }
    let res = s.call_tool("sidekick_run", json!({"prompt": "hi", "cwd": cwd, "backend": "cl", "model": "Sonnet[1m]"}));
    assert!(text_of(&res).contains("needs `dimension`"), "{}", text_of(&res));
}
