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
    std::fs::write(
        cfg.join("config.toml"),
        format!("[backends.cmd]\ntype = \"command\"\ndescription = \"t\"\ndefault = true\ncommand = ['{}', '{{prompt}}']\n", script.display()),
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
