//! `agent-sidekick setup --codex` and `uninstall` against a Codex config in a temporary
//! `CODEX_HOME` (never the user's real `~/.codex`; `HOME`/`USERPROFILE` point to a temporary
//! directory as well). Codex itself is not started.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A user Codex config with comments and another server: must survive byte for byte.
const USER_CONFIG: &str = "# my codex config\nmodel = \"gpt-6.1-sol\" # favourite\n\n\
[mcp_servers.other]\ncommand = \"foo\" # inline comment\nargs = [\"a\",   \"b\"]\n";

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["home", "cfg", "codex-home"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        Self { dir }
    }

    fn codex_config(&self) -> PathBuf {
        self.dir.path().join("codex-home").join("config.toml")
    }

    /// A fake `claude` that accepts every management command (`mcp remove` in uninstall).
    fn fake_claude(&self) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = self.dir.path().join("claude");
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }
        #[cfg(windows)]
        {
            let p = self.dir.path().join("claude.cmd");
            std::fs::write(&p, "@echo off\r\nexit /b 0\r\n").unwrap();
            p
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.dir.path().join("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-sidekick"));
        cmd.args(args);
        for (k, _) in std::env::vars_os() {
            let up = k.to_string_lossy().to_ascii_uppercase();
            if up.starts_with("AGENT_SIDEKICK_") || up.starts_with("CODEX_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("CODEX_HOME", self.dir.path().join("codex-home"))
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("AGENT_SIDEKICK_CONFIG_DIR", self.dir.path().join("cfg"))
            .env("AGENT_SIDEKICK_DISABLE_KEYRING", "1")
            .env("AGENT_SIDEKICK_CLAUDE", self.fake_claude());
        cmd.output().expect("run agent-sidekick")
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains(".bak-agent-sidekick-"))
        .collect();
    v.sort();
    v
}

#[test]
fn setup_codex_registers_and_uninstall_removes() {
    let env = Env::new();
    std::fs::write(env.codex_config(), USER_CONFIG).unwrap();
    std::fs::write(
        env.dir.path().join("cfg").join("config.toml"),
        "[backends.gpt]\nharness = \"codex\"\nauth = \"api-key\"\napi_key_env = \"MY_OAI_KEY\"\n\
         description = \"d\"\ndefault = true\n",
    )
    .unwrap();

    let out = env.run(&["setup", "--codex"]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("Registered MCP server `agent-sidekick` in Codex"),
        "{t}"
    );
    assert!(t.contains("tool_timeout_sec 15000"), "{t}");
    assert!(t.contains("Backup of the previous Codex config"), "{t}");

    let written = std::fs::read_to_string(env.codex_config()).unwrap();
    assert!(written.starts_with(USER_CONFIG), "{written}");
    let v: toml::Value = toml::from_str(&written).unwrap();
    let e = &v["mcp_servers"]["agent-sidekick"];
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_agent-sidekick"));
    assert_eq!(
        PathBuf::from(e["command"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        exe.canonicalize().unwrap()
    );
    assert_eq!(e["args"], toml::Value::Array(vec!["serve".into()]));
    assert_eq!(e["tool_timeout_sec"].as_integer(), Some(15_000));
    assert_eq!(
        PathBuf::from(e["env"]["AGENT_SIDEKICK_CLAUDE"].as_str().unwrap()),
        env.fake_claude()
    );
    let vars: Vec<&str> = e["env_vars"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert!(vars.contains(&"MY_OAI_KEY"), "{vars:?}");
    let backups_now = backups(&env.dir.path().join("codex-home"));
    assert_eq!(backups_now.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&backups_now[0]).unwrap(),
        USER_CONFIG
    );

    // Again: nothing changes, no new backup.
    let out = env.run(&["setup", "--codex"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("nothing changed"), "{}", text(&out));
    assert_eq!(backups(&env.dir.path().join("codex-home")).len(), 1);

    // doctor sees it (Codex may or may not be installed on the test machine: only checked
    // when the line is there).
    let out = env.run(&["doctor", "--offline"]);
    let t = text(&out);
    if t.contains("Codex MCP registration") {
        assert!(
            t.contains("[OK]   Codex MCP registration: registered in"),
            "{t}"
        );
    }

    // uninstall removes only our entry.
    let out = env.run(&["uninstall"]);
    let t = text(&out);
    assert!(
        t.contains("Removed MCP server `agent-sidekick` from Codex"),
        "{t}"
    );
    let after = std::fs::read_to_string(env.codex_config()).unwrap();
    assert_eq!(after.trim_end(), USER_CONFIG.trim_end());
    let out = env.run(&["uninstall"]);
    assert!(
        text(&out).contains("was not registered in Codex"),
        "{}",
        text(&out)
    );
    // The fake home was never written to.
    assert_eq!(
        std::fs::read_dir(env.dir.path().join("home"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn setup_codex_creates_a_missing_codex_config() {
    let env = Env::new();
    let out = env.run(&["setup", "--codex"]);
    assert!(out.status.success(), "{}", text(&out));
    let written = std::fs::read_to_string(env.codex_config()).unwrap();
    assert!(
        written.starts_with("[mcp_servers.agent-sidekick]\n"),
        "{written}"
    );
    assert!(backups(&env.dir.path().join("codex-home")).is_empty());
}

#[test]
fn setup_codex_refuses_an_inline_mcp_servers_table() {
    let env = Env::new();
    let original = "mcp_servers = { other = { command = \"foo\" } }\n";
    std::fs::write(env.codex_config(), original).unwrap();
    let out = env.run(&["setup", "--codex"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("not a [table]"), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(env.codex_config()).unwrap(),
        original
    );
    assert!(backups(&env.dir.path().join("codex-home")).is_empty());
}
