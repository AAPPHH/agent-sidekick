//! Black-box tests of the codex harness through `agent-sidekick serve`, with a FAKE codex
//! (a generated sh script on Unix, a .cmd script on Windows, configured through
//! `harness_path`) and the fake HTTP API of tests/support/fake_api.rs as the provider for
//! the pre-run probe. No real Codex, no model, no network beyond 127.0.0.1.
//!
//! The fake records its argv, the NAMES of its environment variables (never values; only
//! the non-secret `CODEX_HOME`/`CODEX_SQLITE_HOME` are recorded with values), stdin, its
//! working directory and a copy of the generated config.toml, writes a file into
//! `CODEX_HOME` (as Codex would), and prints canned `exec --json` lines.

#[path = "support/fake_api.rs"]
mod fake_api;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fake_api::{log_path_of, text_of, FakeApi, Server};
use serde_json::{json, Value};

const RUN_WITHIN: Duration = Duration::from_secs(60);
const KEY: &str = "sk-proj-fake-codex-key-0123456789abcdef";
const OAUTH: &str = "sk-ant-oat01-CODEXTESTTOKEN-0123456789abcdef";
/// Part of [`OAUTH`] that redaction of `sk-ant-` prefixes alone would not hide.
const OAUTH_SUFFIX: &str = "CODEXTESTTOKEN-0123456789abcdef";
const ANSWER: &str = "FAKE-CODEX-ANSWER";

#[cfg(unix)]
fn fake_codex(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let body = "#!/bin/sh\n\
         out=\"$FAKE_CX_OUT\"\n\
         mkdir -p \"$out\"\n\
         printf '%s\\n' \"$@\" > \"$out/argv.txt\"\n\
         env | cut -d= -f1 | sort > \"$out/env-names.txt\"\n\
         cat > \"$out/stdin.txt\"\n\
         pwd -P > \"$out/cwd.txt\"\n\
         printf 'CODEX_HOME=%s\\nCODEX_SQLITE_HOME=%s\\n' \"$CODEX_HOME\" \"$CODEX_SQLITE_HOME\" > \"$out/iso.txt\"\n\
         [ -d \"$CODEX_HOME\" ] || exit 9\n\
         cp \"$CODEX_HOME/config.toml\" \"$out/config.toml\"\n\
         : > \"$CODEX_HOME/written-by-fake-codex\"\n\
         if [ -n \"$CODEX_API_KEY\" ]; then echo yes > \"$out/codex-key.txt\"; fi\n\
         if [ -n \"$AGENT_SIDEKICK_PROVIDER_KEY\" ]; then echo yes > \"$out/provider-key.txt\"; fi\n\
         echo 'WARNING: proceeding, even though we could not create PATH aliases: test' 1>&2\n\
         cat \"$FAKE_CX_CANNED\"\n\
         exit \"${FAKE_CX_EXIT:-0}\"\n";
    let path = dir.join("codex");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
fn fake_codex(dir: &Path) -> PathBuf {
    let body = "@echo off\n\
         setlocal\n\
         set \"out=%FAKE_CX_OUT%\"\n\
         if not exist \"%out%\" mkdir \"%out%\"\n\
         >\"%out%\\argv.txt\" echo %*\n\
         for /f \"tokens=1 delims==\" %%k in ('set') do >>\"%out%\\env-names.txt\" echo %%k\n\
         findstr \"^\" > \"%out%\\stdin.txt\"\n\
         >\"%out%\\cwd.txt\" cd\n\
         >\"%out%\\iso.txt\" echo CODEX_HOME=%CODEX_HOME%\n\
         >>\"%out%\\iso.txt\" echo CODEX_SQLITE_HOME=%CODEX_SQLITE_HOME%\n\
         if not exist \"%CODEX_HOME%\" exit /b 9\n\
         copy /y \"%CODEX_HOME%\\config.toml\" \"%out%\\config.toml\" >nul\n\
         type nul > \"%CODEX_HOME%\\written-by-fake-codex\"\n\
         if defined CODEX_API_KEY >\"%out%\\codex-key.txt\" echo yes\n\
         if defined AGENT_SIDEKICK_PROVIDER_KEY >\"%out%\\provider-key.txt\" echo yes\n\
         echo WARNING: proceeding, even though we could not create PATH aliases: test 1>&2\n\
         type \"%FAKE_CX_CANNED%\"\n\
         if defined FAKE_CX_EXIT exit /b %FAKE_CX_EXIT%\n\
         exit /b 0\n";
    let path = dir.join("codex.cmd");
    std::fs::write(&path, body.replace('\n', "\r\n")).unwrap();
    path
}

/// A command backend script that prints `GOOD-RAN` (prompt via stdin on Windows).
fn good_backend(dir: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("good.sh");
        std::fs::write(&path, "#!/bin/sh\necho GOOD-RAN\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!(
            "[backends.good]\nharness = \"command\"\ndescription = \"good\"\n\
             command = ['{}', '{{prompt}}']\n",
            path.display()
        )
    }
    #[cfg(windows)]
    {
        let path = dir.join("good.cmd");
        std::fs::write(
            &path,
            "@echo off\r\necho GOOD-RAN\r\nfindstr \"^\" >nul\r\n",
        )
        .unwrap();
        format!(
            "[backends.good]\nharness = \"command\"\ndescription = \"good\"\n\
             command = ['{}']\nprompt_via_stdin = true\n",
            path.display()
        )
    }
}

fn ev(v: Value) -> String {
    v.to_string()
}

fn answer_lines(text: &str) -> String {
    [
        ev(json!({"type":"thread.started","thread_id":"t1"})),
        ev(json!({"type":"turn.started"})),
        ev(json!({"type":"error","message":"Reconnecting... 1/5 (stream disconnected)"})),
        ev(json!({"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"cargo test","aggregated_output":"ok","exit_code":0,"status":"completed"}})),
        ev(json!({"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":text}})),
        ev(json!({"type":"turn.completed","usage":{"input_tokens":246,"cached_input_tokens":14,"cache_write_input_tokens":0,"output_tokens":90,"reasoning_output_tokens":6}})),
    ]
    .join("\n")
        + "\n"
}

fn failed_lines() -> String {
    [
        ev(json!({"type":"thread.started","thread_id":"t1"})),
        ev(json!({"type":"turn.started"})),
        ev(json!({"type":"error","message":"unexpected status 401 Unauthorized"})),
        ev(json!({"type":"turn.failed","error":{"message":"unexpected status 401 Unauthorized"}})),
    ]
    .join("\n")
        + "\n"
}

struct Fixture {
    dir: tempfile::TempDir,
    config_dir: PathBuf,
    work: PathBuf,
    rec: PathBuf,
    real_home: PathBuf,
    fake: PathBuf,
    api: FakeApi,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("cfg");
        let work = dir.path().join("work");
        let rec = dir.path().join("rec");
        let real_home = dir.path().join("real-home");
        let bin = dir.path().join("bin");
        for d in [&config_dir, &work, &real_home, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        let fake = fake_codex(&bin);
        let api = FakeApi::start_with_canaries(&[KEY, OAUTH_SUFFIX]);
        Self {
            dir,
            config_dir,
            work,
            rec,
            real_home,
            fake,
            api,
        }
    }

    /// `[backends.cx]` running the fake with `canned` output and exit code; `auth` lines
    /// (`auth = ...`, `base_url = ...`).
    fn cx_backend(&self, default: bool, auth: &str, canned: &str, exit: u8) -> String {
        let canned_path = self.dir.path().join(format!("canned-{exit}.jsonl"));
        std::fs::write(&canned_path, canned).unwrap();
        format!(
            "[backends.cx]\nharness = \"codex\"\n{auth}\ndescription = \"fake codex\"\n\
             default = {default}\nharness_path = '{}'\n\
             [backends.cx.env]\nFAKE_CX_OUT = '{}'\nFAKE_CX_CANNED = '{}'\nFAKE_CX_EXIT = \"{exit}\"\n",
            self.fake.display(),
            self.rec.display(),
            canned_path.display()
        )
    }

    fn home(&self) -> PathBuf {
        self.config_dir.join("codex").join("cx")
    }

    fn login(&self) {
        std::fs::create_dir_all(self.home()).unwrap();
        std::fs::write(self.home().join("auth.json"), "{\"fake\":true}").unwrap();
    }

    fn start(&self, config: &str, extra: &[(&str, &str)]) -> Server {
        std::fs::write(self.config_dir.join("config.toml"), config).unwrap();
        std::fs::write(self.config_dir.join("backend-cx.key"), KEY).unwrap();
        let home = self.real_home.to_string_lossy().into_owned();
        let real_codex = self.real_home.join(".codex").to_string_lossy().into_owned();
        let mut env: Vec<(&str, &str)> = vec![
            ("HOME", home.as_str()),
            ("USERPROFILE", home.as_str()),
            ("CLAUDE_CODE_OAUTH_TOKEN", OAUTH),
            ("ANTHROPIC_API_KEY", "sk-ant-api03-caller-0123456789"),
            // A Codex host's session: none of it may reach the child.
            ("CODEX_SESSION_ID", "host-session"),
            ("CODEX_THREAD_ID", "host-thread"),
            ("CODEX_SANDBOX_NETWORK_DISABLED", "1"),
            ("OPENAI_API_KEY", "sk-caller-openai-0123456789"),
            ("RUST_LOG", "trace"),
        ];
        // Inherited by the server as the user's real codex home: never used.
        env.push(("CODEX_HOME", real_codex.as_str()));
        env.extend_from_slice(extra);
        Server::start(&self.config_dir, &env)
    }

    fn run(&self, server: &mut Server, args: Value) -> (bool, String) {
        let mut args = args;
        args["cwd"] = json!(self.work.to_string_lossy());
        let res = server.call_tool("sidekick_run", args, RUN_WITHIN);
        (res["isError"] == json!(true), text_of(&res))
    }

    fn rec_file(&self, name: &str) -> String {
        std::fs::read_to_string(self.rec.join(name))
            .unwrap_or_else(|e| panic!("{name} not recorded: {e}"))
    }

    fn env_names(&self) -> BTreeSet<String> {
        self.rec_file("env-names.txt")
            .lines()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Every file below `dir` (relative paths).
fn files_below(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    out
}

fn has_name(names: &BTreeSet<String>, n: &str) -> bool {
    names.iter().any(|x| {
        if cfg!(windows) {
            x.eq_ignore_ascii_case(n)
        } else {
            x == n
        }
    })
}

/// No secret value in any file below `dir` (the key file of the file store excepted).
fn assert_no_secrets_below(dir: &Path) {
    for rel in files_below(dir) {
        if rel == Path::new("backend-cx.key") {
            continue;
        }
        let bytes = std::fs::read(dir.join(&rel)).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(KEY), "API key in {}", rel.display());
        assert!(
            !text.contains(OAUTH_SUFFIX),
            "OAuth token in {}",
            rel.display()
        );
    }
}

/// The recorded argv (on Windows `%*` of the .cmd fake, where Rust quotes arguments with
/// `=` for cmd.exe: the quotes are removed here).
fn argv(f: &Fixture) -> Vec<String> {
    f.rec_file("argv.txt")
        .split_whitespace()
        .map(|a| a.trim_matches('"').to_owned())
        .collect()
}

#[test]
fn chatgpt_run_is_isolated_normalized_and_leaks_nothing() {
    let f = Fixture::new();
    f.login();
    let mut server = f.start(
        &f.cx_backend(true, "auth = \"chatgpt\"", &answer_lines(ANSWER), 0),
        &[],
    );
    let before = files_below(&f.config_dir);
    let (is_error, text) = f.run(
        &mut server,
        json!({"prompt": "Explain this and that", "model": "gpt-6.1-sol", "max_turns": 9}),
    );
    assert!(!is_error, "{text}");

    // Result: activity and warnings first, then the answer, then usage.
    assert!(
        text.starts_with("backend: cx (codex), model gpt-6.1-sol\n"),
        "{text}"
    );
    assert!(text.contains("exit_code: 0"), "{text}");
    let output = text.split("---\n").nth(1).unwrap_or_default();
    assert_eq!(
        output,
        format!(
            "[codex activity] 1 command(s), 0 MCP tool call(s)\n\
             [command] cargo test -> exit 0 (completed)\n\
             [warning] Reconnecting... 1/5 (stream disconnected)\n\
             {ANSWER}\n\
             [usage] tokens in 246 (cached 14) / out 90 (reasoning 6)"
        ),
        "{text}"
    );

    // argv and stdin.
    let mut want: Vec<String> = [
        "exec",
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--color",
        "never",
        "--sandbox",
        "workspace-write",
        "--cd",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    want.push(f.work.to_string_lossy().into_owned());
    want.extend(["-m".to_owned(), "gpt-6.1-sol".to_owned()]);
    if cfg!(windows) {
        want.extend(["-c".to_owned(), "windows.sandbox=mxc".to_owned()]);
    }
    want.push("-".to_owned());
    assert_eq!(argv(&f), want);
    assert_eq!(f.rec_file("stdin.txt").trim_end(), "Explain this and that");
    let cwd = PathBuf::from(f.rec_file("cwd.txt").trim());
    assert_eq!(
        cwd.canonicalize().unwrap(),
        f.work.canonicalize().unwrap(),
        "cwd"
    );

    // CODEX_HOME and CODEX_SQLITE_HOME are the backend's own directory, never the inherited
    // (real) one.
    for line in f.rec_file("iso.txt").lines() {
        let (name, value) = line.split_once('=').unwrap();
        assert_eq!(PathBuf::from(value.trim()), f.home(), "{name}");
    }
    let names = f.env_names();
    for gone in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "AGENT_SIDEKICK_TOKEN",
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_API_KEY",
        "OPENAI_API_KEY",
        "RUST_LOG",
        "AGENT_SIDEKICK_PROVIDER_KEY",
    ] {
        assert!(!has_name(&names, gone), "{gone} reached codex");
    }
    assert!(!f.rec.join("codex-key.txt").exists());
    assert!(!f.rec.join("provider-key.txt").exists());

    // The generated config: file credential store, shell filters, no provider block.
    let cfg: toml::Value = toml::from_str(&f.rec_file("config.toml")).unwrap();
    assert_eq!(cfg["cli_auth_credentials_store"].as_str(), Some("file"));
    assert_eq!(
        cfg["shell_environment_policy"]["ignore_default_excludes"].as_bool(),
        Some(false)
    );
    assert_eq!(
        cfg["shell_environment_policy"]["filters"]["AGENT_SIDEKICK_*"].as_str(),
        Some("exclude")
    );
    assert!(cfg.get("model_providers").is_none());

    // Files: new ones only in logs/ and codex/cx/; nothing in the (fake) real home.
    let after = files_below(&f.config_dir);
    let outside: Vec<&PathBuf> = after
        .difference(&before)
        .filter(|p| !p.starts_with("logs") && !p.starts_with(Path::new("codex").join("cx")))
        .collect();
    assert!(
        outside.is_empty(),
        "files outside the backend dir: {outside:?}"
    );
    assert!(
        files_below(&f.real_home).is_empty(),
        "written into the real home: {:?}",
        files_below(&f.real_home)
    );
    assert!(f.home().join("written-by-fake-codex").is_file());

    // ChatGPT mode: no provider probe, no secret anywhere.
    assert!(f.api.records().is_empty(), "{:?}", f.api.records());
    assert_no_secrets_below(&f.config_dir);
    assert_no_secrets_below(&f.rec);
    let log = std::fs::read_to_string(log_path_of(&text).expect("log path")).unwrap();
    assert!(log.contains("# auth: chatgpt"), "{log}");
    assert!(log.contains("# max_turns: 9 ignored"), "{log}");
    assert_eq!(log.matches("# max_turns:").count(), 1, "{log}");
    assert!(!log.contains(OAUTH_SUFFIX));
}

#[test]
fn api_key_run_with_a_responses_provider() {
    let f = Fixture::new();
    let auth = format!("auth = \"api-key\"\nbase_url = '{}/v1'", f.api.base_url);
    let mut server = f.start(
        &f.cx_backend(true, &auth, &answer_lines(&format!("the key is {KEY}")), 0),
        &[],
    );
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(!is_error, "{text}");
    // An echoed key is redacted in the result and the log.
    assert!(text.contains("the key is [REDACTED]"), "{text}");
    assert!(!text.contains(KEY), "{text}");
    let log = std::fs::read_to_string(log_path_of(&text).unwrap()).unwrap();
    assert!(!log.contains(KEY));
    assert!(log.contains("# model: codex default"), "{log}");
    assert_eq!(log.matches("# model:").count(), 1, "{log}");

    // The custom provider reads AGENT_SIDEKICK_PROVIDER_KEY; CODEX_API_KEY stays unset.
    assert_eq!(f.rec_file("provider-key.txt").trim(), "yes");
    assert!(!f.rec.join("codex-key.txt").exists());
    assert!(!argv(&f).iter().any(|a| a == "-m"));
    let cfg: toml::Value = toml::from_str(&f.rec_file("config.toml")).unwrap();
    assert_eq!(cfg["model_provider"].as_str(), Some("sidekick"));
    assert_eq!(
        cfg["model_providers"]["sidekick"]["base_url"].as_str(),
        Some(format!("{}/v1", f.api.base_url).as_str())
    );
    assert_eq!(
        cfg["model_providers"]["sidekick"]["env_key"].as_str(),
        Some("AGENT_SIDEKICK_PROVIDER_KEY")
    );

    // The provider probe: one GET of /v1/models with the key as bearer token.
    let records = f.api.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        (records[0].method.as_str(), records[0].path.as_str()),
        ("GET", "/v1/models")
    );
    let (prefix, len) = records[0].authorization.clone().expect("authorization");
    assert!(prefix.starts_with("Bearer sk-proj-fake"), "{prefix}");
    assert_eq!(len, "Bearer ".len() + KEY.len());
    assert!(!records[0].canaries_seen.iter().any(|c| c == OAUTH_SUFFIX));
    assert_no_secrets_below(&f.config_dir);
    assert_no_secrets_below(&f.rec);
}

#[test]
fn not_logged_in_fails_before_start_and_falls_back() {
    let f = Fixture::new();
    let config = format!(
        "{}\n{}",
        f.cx_backend(true, "auth = \"chatgpt\"", &answer_lines(ANSWER), 0),
        good_backend(f.dir.path())
    );
    let mut server = f.start(&config, &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(is_error, "{text}");
    assert!(text.contains("No ChatGPT login for backend 'cx'"), "{text}");
    assert!(text.contains("agent-sidekick setup --backend cx"), "{text}");
    assert!(!f.rec.join("argv.txt").exists(), "codex must not start");

    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi", "fallback": "good"}));
    assert!(!is_error, "{text}");
    assert!(text.starts_with("backend: good (command)"), "{text}");
    assert!(text.contains("GOOD-RAN"), "{text}");
}

#[test]
fn turn_failed_is_reported_and_falls_back() {
    let f = Fixture::new();
    f.login();
    let config = format!(
        "{}\n{}",
        f.cx_backend(true, "auth = \"chatgpt\"", &failed_lines(), 1),
        good_backend(f.dir.path())
    );
    let mut server = f.start(&config, &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(is_error, "{text}");
    assert!(text.contains("exit_code: 1"), "{text}");
    assert!(
        text.contains("[error] turn failed: unexpected status 401 Unauthorized"),
        "{text}"
    );

    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi", "fallback": "good"}));
    assert!(!is_error, "{text}");
    assert!(
        text.starts_with("backend: good (command) — fallback after 'cx' failed: exit_code 1\n"),
        "{text}"
    );
}

/// Exit 0 without a final message is still a failure (and eligible for a fallback).
#[test]
fn no_answer_counts_as_failure() {
    let f = Fixture::new();
    f.login();
    let canned = ev(json!({"type":"turn.completed","usage":{"input_tokens":1}})) + "\n";
    let mut server = f.start(&f.cx_backend(true, "auth = \"chatgpt\"", &canned, 0), &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(is_error, "{text}");
    assert!(
        text.contains("failed: no answer: codex produced no final message"),
        "{text}"
    );
}

/// More output than the runner's in-memory buffer: the answer is still found, because the
/// result is computed from the complete output in the run log.
#[test]
fn long_runs_are_normalized_from_the_log() {
    let padding = "x".repeat(1000);
    let mut canned = String::new();
    for i in 0..300 {
        canned.push_str(&ev(json!({"type":"item.completed","item":{"type":"command_execution","command":format!("step {i}"),"aggregated_output":padding,"exit_code":0,"status":"completed"}})));
        canned.push('\n');
    }
    canned.push_str(&answer_lines(ANSWER));
    assert!(canned.len() > 300 * 1024, "must exceed the runner's buffer");
    let f = Fixture::new();
    f.login();
    let mut server = f.start(&f.cx_backend(true, "auth = \"chatgpt\"", &canned, 0), &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(!is_error, "{text}");
    assert!(
        text.contains("[codex activity] 301 command(s), 0 MCP tool call(s)"),
        "{text}"
    );
    assert!(text.contains(ANSWER), "{text}");
}

#[test]
fn claude_falls_back_to_codex_and_the_oauth_token_stays_behind() {
    let f = Fixture::new();
    f.login();
    #[cfg(unix)]
    let claude = {
        use std::os::unix::fs::PermissionsExt;
        let path = f.dir.path().join("claude");
        std::fs::write(&path, "#!/bin/sh\necho CLAUDE-FAILED\nexit 3\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    #[cfg(windows)]
    let claude = {
        let path = f.dir.path().join("claude.cmd");
        std::fs::write(&path, "@echo off\r\necho CLAUDE-FAILED\r\nexit /b 3\r\n").unwrap();
        path
    };
    let config = format!(
        "[backends.sidekick]\nharness = \"claude\"\ndescription = \"claude account\"\n\
         default = true\nconfig_dir = '{}'\n\n{}",
        f.dir.path().join("claude-cfg").display(),
        f.cx_backend(false, "auth = \"chatgpt\"", &answer_lines(ANSWER), 0)
    );
    let claude_s = claude.to_string_lossy().into_owned();
    let mut server = f.start(
        &config,
        &[
            ("AGENT_SIDEKICK_TOKEN", OAUTH),
            ("AGENT_SIDEKICK_CLAUDE", claude_s.as_str()),
        ],
    );
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi", "fallback": "cx"}));
    assert!(!is_error, "{text}");
    assert!(
        text.starts_with("backend: cx (codex) — fallback after 'sidekick' failed: exit_code 3\n"),
        "{text}"
    );
    assert!(text.contains(ANSWER), "{text}");
    let names = f.env_names();
    for gone in ["CLAUDE_CODE_OAUTH_TOKEN", "AGENT_SIDEKICK_TOKEN"] {
        assert!(!has_name(&names, gone), "{gone} reached codex");
    }
    assert_no_secrets_below(&f.rec);
}

#[test]
fn backends_and_status_report_codex() {
    let f = Fixture::new();
    let mut server = f.start(
        &f.cx_backend(true, "auth = \"chatgpt\"", &answer_lines(ANSWER), 0),
        &[],
    );
    let res = server.call_tool("sidekick_backends", json!({}), RUN_WITHIN);
    let text = text_of(&res);
    assert!(text.contains("cx"), "{text}");
    assert!(text.contains("not logged in"), "{text}");
    f.login();
    let text = text_of(&server.call_tool("sidekick_backends", json!({}), RUN_WITHIN));
    assert!(text.contains("ChatGPT login present"), "{text}");
    let text = text_of(&server.call_tool("sidekick_status", json!({}), RUN_WITHIN));
    assert!(text.contains("cx (codex, default)"), "{text}");
    assert!(text.contains("model (codex default)"), "{text}");
}
