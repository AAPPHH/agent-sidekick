//! Black-box tests of the opencode harness through `agent-sidekick serve`, with a FAKE
//! opencode (a generated sh script on Unix, a .cmd script on Windows, configured through
//! `harness_path`) and the fake HTTP API of tests/support/fake_api.rs as the provider for
//! the pre-run probe. No real OpenCode, no model, no network beyond 127.0.0.1.
//!
//! The fake records its argv, the NAMES of its environment variables (never values; only
//! the non-secret isolation paths are recorded with values), stdin, its working directory
//! and a copy of the generated opencode.json, writes a file into every isolation directory
//! (as OpenCode would), and prints canned `--format json` lines.

#[path = "support/fake_api.rs"]
mod fake_api;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fake_api::{log_path_of, text_of, FakeApi, Server};
use serde_json::{json, Value};

const RUN_WITHIN: Duration = Duration::from_secs(60);
const KEY: &str = "kic-fake-provider-key-0123456789abcdef";
const OAUTH: &str = "sk-ant-oat01-OPENCODETESTTOKEN-0123456789abcdef";
/// Part of [`OAUTH`] that redaction of `sk-ant-` prefixes alone would not hide.
const OAUTH_SUFFIX: &str = "OPENCODETESTTOKEN-0123456789abcdef";
const ANSWER: &str = "FAKE-OPENCODE-ANSWER";

/// Isolation variables the fake records with their (non-secret) values.
const ISO_VARS: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "TEMP",
    "TMP",
    "npm_config_cache",
];

#[cfg(unix)]
fn fake_opencode(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut iso = String::new();
    for v in ISO_VARS {
        iso.push_str(&format!(
            "printf '%s=%s\\n' {v} \"${v}\" >> \"$out/iso.txt\"\n"
        ));
    }
    let body = format!(
        "#!/bin/sh\n\
         out=\"$FAKE_OC_OUT\"\n\
         mkdir -p \"$out\"\n\
         printf '%s\\n' \"$@\" > \"$out/argv.txt\"\n\
         env | cut -d= -f1 | sort > \"$out/env-names.txt\"\n\
         cat > \"$out/stdin.txt\"\n\
         printf '%s\\n' \"$PWD\" > \"$out/pwd.txt\"\n\
         pwd -P > \"$out/cwd.txt\"\n\
         cp \"$XDG_CONFIG_HOME/opencode/opencode.json\" \"$out/opencode.json\"\n\
         {iso}\
         if [ -n \"$AGENT_SIDEKICK_PROVIDER_KEY\" ]; then echo yes > \"$out/key-present.txt\"; fi\n\
         for d in \"$HOME\" \"$XDG_DATA_HOME\" \"$XDG_STATE_HOME\" \"$XDG_CACHE_HOME\" \"$TMP\" \"$npm_config_cache\"; do\n\
           : > \"$d/written-by-fake-opencode\"\n\
         done\n\
         cat \"$FAKE_OC_CANNED\"\n\
         exit \"${{FAKE_OC_EXIT:-0}}\"\n"
    );
    let path = dir.join("opencode");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
fn fake_opencode(dir: &Path) -> PathBuf {
    let mut iso = String::new();
    for v in ISO_VARS
        .iter()
        .chain(&["USERPROFILE", "APPDATA", "LOCALAPPDATA"])
    {
        iso.push_str(&format!(">>\"%out%\\iso.txt\" echo {v}=%{v}%\n"));
    }
    let body = format!(
        "@echo off\n\
         setlocal\n\
         set \"out=%FAKE_OC_OUT%\"\n\
         if not exist \"%out%\" mkdir \"%out%\"\n\
         >\"%out%\\argv.txt\" echo %*\n\
         for /f \"tokens=1 delims==\" %%k in ('set') do >>\"%out%\\env-names.txt\" echo %%k\n\
         findstr \"^\" > \"%out%\\stdin.txt\"\n\
         >\"%out%\\cwd.txt\" cd\n\
         copy /y \"%XDG_CONFIG_HOME%\\opencode\\opencode.json\" \"%out%\\opencode.json\" >nul\n\
         {iso}\
         if defined AGENT_SIDEKICK_PROVIDER_KEY >\"%out%\\key-present.txt\" echo yes\n\
         for %%d in (\"%USERPROFILE%\" \"%APPDATA%\" \"%LOCALAPPDATA%\" \"%XDG_DATA_HOME%\" \"%XDG_STATE_HOME%\" \"%XDG_CACHE_HOME%\" \"%TMP%\" \"%npm_config_cache%\") do type nul > \"%%~d\\written-by-fake-opencode\"\n\
         type \"%FAKE_OC_CANNED%\"\n\
         if defined FAKE_OC_EXIT exit /b %FAKE_OC_EXIT%\n\
         exit /b 0\n"
    );
    let path = dir.join("opencode.cmd");
    std::fs::write(&path, body.replace('\n', "\r\n")).unwrap();
    path
}

/// A command backend script that prints `GOOD-RAN` (prompt via stdin on Windows).
fn good_script(dir: &Path) -> (PathBuf, String) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("good.sh");
        std::fs::write(&path, "#!/bin/sh\necho GOOD-RAN\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = path.to_string_lossy().into_owned();
        (path, format!("command = ['{p}', '{{prompt}}']\n"))
    }
    #[cfg(windows)]
    {
        let path = dir.join("good.cmd");
        std::fs::write(
            &path,
            "@echo off\r\necho GOOD-RAN\r\nfindstr \"^\" >nul\r\n",
        )
        .unwrap();
        let p = path.to_string_lossy().into_owned();
        (
            path,
            format!("command = ['{p}']\nprompt_via_stdin = true\n"),
        )
    }
}

/// A fake `claude` that exits 3 (so a fallback runs).
fn failing_claude(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        std::fs::write(&path, "#!/bin/sh\necho CLAUDE-FAILED\nexit 3\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(windows)]
    {
        let path = dir.join("claude.cmd");
        std::fs::write(&path, "@echo off\r\necho CLAUDE-FAILED\r\nexit /b 3\r\n").unwrap();
        path
    }
}

fn ev(v: Value) -> String {
    v.to_string()
}

fn answer_lines(text: &str) -> String {
    [
        ev(json!({"type":"step_start","sessionID":"s1","part":{"type":"step-start"}})),
        ev(json!({"type":"text","sessionID":"s1","part":{"type":"text","text":"thinking about it"}})),
        ev(json!({"type":"step_finish","sessionID":"s1","part":{"reason":"tool-calls","tokens":{"input":1200,"output":30},"cost":0.0}})),
        ev(json!({"type":"step_start","sessionID":"s1","part":{"type":"step-start"}})),
        ev(json!({"type":"text","sessionID":"s1","part":{"type":"text","text":text}})),
        ev(json!({"type":"step_finish","sessionID":"s1","part":{"reason":"stop","tokens":{"input":1300,"output":12},"cost":0.0}})),
    ]
    .join("\n")
        + "\n"
}

fn rejected_lines() -> String {
    [
        ev(json!({"type":"step_start","sessionID":"s1","part":{"type":"step-start"}})),
        ev(json!({"type":"tool_use","sessionID":"s1","part":{"tool":"read","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}})),
    ]
    .join("\n")
        + "\n"
}

fn error_lines() -> String {
    ev(
        json!({"type":"error","sessionID":"s1","error":{"name":"APIError","data":{"message":"model does not support tools","statusCode":400}}}),
    ) + "\n"
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
        let fake = fake_opencode(&bin);
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

    /// `[backends.oc]` running the fake with `canned` output and exit code.
    fn oc_backend(&self, default: bool, canned: &str, exit: u8) -> String {
        let canned_path = self.dir.path().join(format!("canned-{}.jsonl", exit));
        std::fs::write(&canned_path, canned).unwrap();
        format!(
            "[backends.oc]\nharness = \"opencode\"\nprovider = \"openai-compatible\"\n\
             base_url = '{}/api/v1'\nmodels = [\"m1\", \"m2\"]\ndescription = \"fake opencode\"\n\
             default = {default}\nharness_path = '{}'\n\
             [backends.oc.env]\nFAKE_OC_OUT = '{}'\nFAKE_OC_CANNED = '{}'\nFAKE_OC_EXIT = \"{exit}\"\n",
            self.api.base_url,
            self.fake.display(),
            self.rec.display(),
            canned_path.display()
        )
    }

    fn start(&self, config: &str, extra: &[(&str, &str)]) -> Server {
        std::fs::write(self.config_dir.join("config.toml"), config).unwrap();
        std::fs::write(self.config_dir.join("backend-oc.key"), KEY).unwrap();
        let home = self.real_home.to_string_lossy().into_owned();
        let mut env: Vec<(&str, &str)> = vec![
            ("HOME", home.as_str()),
            ("USERPROFILE", home.as_str()),
            ("CLAUDE_CODE_OAUTH_TOKEN", OAUTH),
            ("ANTHROPIC_API_KEY", "sk-ant-api03-caller-0123456789"),
            ("OPENCODE_CONFIG", "/evil/opencode.json"),
            ("OPENCODE_PERMISSION", "{\"bash\":\"ask\"}"),
            ("PWD", "/bogus/pwd"),
        ];
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

    fn base(&self) -> PathBuf {
        self.config_dir.join("opencode").join("oc")
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

/// No secret value in any file below `dir`.
fn assert_no_secrets_below(dir: &Path) {
    for rel in files_below(dir) {
        let bytes = std::fs::read(dir.join(&rel)).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(KEY), "provider key in {}", rel.display());
        assert!(
            !text.contains(OAUTH_SUFFIX),
            "OAuth token in {}",
            rel.display()
        );
    }
}

#[test]
fn opencode_run_is_isolated_normalized_and_leaks_nothing() {
    let f = Fixture::new();
    let mut server = f.start(&f.oc_backend(true, &answer_lines(ANSWER), 0), &[]);
    let before = files_below(&f.config_dir);
    let (is_error, text) = f.run(
        &mut server,
        json!({"prompt": "Explain \"this\" & that", "model": "m2", "max_turns": 9}),
    );
    assert!(!is_error, "{text}");

    // Result: the final text only, a usage line, the usual header.
    assert!(
        text.starts_with("backend: oc (opencode), model m2\n"),
        "{text}"
    );
    assert!(text.contains("exit_code: 0"), "{text}");
    let output = text.split("---\n").nth(1).unwrap_or_default();
    assert_eq!(
        output,
        format!("{ANSWER}\n[usage] steps 2, tokens in 2500 / out 42, cost 0.0000"),
        "{text}"
    );

    // argv and stdin.
    let argv = f.rec_file("argv.txt");
    let argv: Vec<&str> = argv.split_whitespace().collect();
    assert_eq!(
        argv,
        [
            "run",
            "--format",
            "json",
            "--model",
            "sidekick/m2",
            "--title",
            "agent-sidekick"
        ]
    );
    assert_eq!(
        f.rec_file("stdin.txt").trim_end(),
        "Explain \"this\" & that"
    );
    // Working directory = the run's cwd; the inherited PWD is gone.
    let cwd = PathBuf::from(f.rec_file("cwd.txt").trim());
    assert_eq!(
        cwd.canonicalize().unwrap(),
        f.work.canonicalize().unwrap(),
        "cwd"
    );
    let names = f.env_names();
    if cfg!(windows) {
        assert!(!has_name(&names, "PWD"), "PWD passed: {names:?}");
    } else {
        // sh sets PWD itself at startup; it must be the real cwd, never the inherited one.
        assert_ne!(f.rec_file("pwd.txt").trim(), "/bogus/pwd");
    }

    // Environment: key present (name only), OAuth token and competing settings absent.
    assert_eq!(f.rec_file("key-present.txt").trim(), "yes");
    for gone in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENCODE_CONFIG",
        "AGENT_SIDEKICK_TOKEN",
    ] {
        assert!(!has_name(&names, gone), "{gone} reached opencode");
    }
    for present in [
        "AGENT_SIDEKICK_PROVIDER_KEY",
        "OPENCODE_PERMISSION",
        "OPENCODE_CONFIG_CONTENT",
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "XDG_CONFIG_HOME",
    ] {
        assert!(has_name(&names, present), "{present} missing: {names:?}");
    }
    // Every isolation path is inside <config-dir>/opencode/oc.
    let base = f.base();
    for line in f.rec_file("iso.txt").lines() {
        let (name, value) = line.split_once('=').unwrap();
        let value = PathBuf::from(value.trim());
        assert!(
            value.starts_with(&base),
            "{name}={} is outside {}",
            value.display(),
            base.display()
        );
    }

    // The generated config: the key only as an env reference.
    let cfg: Value = serde_json::from_str(&f.rec_file("opencode.json")).unwrap();
    let provider = &cfg["provider"]["sidekick"];
    assert_eq!(
        provider["options"]["apiKey"],
        "{env:AGENT_SIDEKICK_PROVIDER_KEY}"
    );
    assert_eq!(
        provider["options"]["baseURL"],
        format!("{}/api/v1", f.api.base_url)
    );
    assert_eq!(cfg["model"], "sidekick/m1");

    // Files: new ones only in logs/ and opencode/oc/; nothing in the (fake) real home.
    let after = files_below(&f.config_dir);
    let outside: Vec<&PathBuf> = after
        .difference(&before)
        .filter(|p| !p.starts_with("logs") && !p.starts_with(Path::new("opencode").join("oc")))
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
    assert!(base.join("home").join("written-by-fake-opencode").is_file());

    // No secret in the recordings, the generated config, the run log or the result.
    assert_no_secrets_below(&f.rec);
    assert_no_secrets_below(&f.base());
    let log = log_path_of(&text).expect("log path");
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(!log_text.contains(KEY) && !log_text.contains(OAUTH_SUFFIX));
    assert!(
        log_text.contains("# provider: openai-compatible"),
        "{log_text}"
    );
    assert!(!text.contains(KEY));

    // The provider probe: one GET of /api/v1/models with the key as bearer token; the key
    // went nowhere else, and the OAuth token never reached the provider.
    let records = f.api.records();
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("GET", "/api/v1/models")
    );
    let (prefix, len) = r.authorization.clone().expect("authorization header");
    assert!(prefix.starts_with("Bearer kic-fake"), "{prefix}");
    assert_eq!(len, "Bearer ".len() + KEY.len());
    assert!(!r.canaries_seen.iter().any(|c| c == OAUTH_SUFFIX));
}

#[test]
fn key_echoed_by_the_agent_is_redacted() {
    let f = Fixture::new();
    let mut server = f.start(
        &f.oc_backend(true, &answer_lines(&format!("the key is {KEY}")), 0),
        &[],
    );
    let (is_error, text) = f.run(&mut server, json!({"prompt": "leak it"}));
    assert!(!is_error, "{text}");
    assert!(!text.contains(KEY), "{text}");
    assert!(text.contains("the key is [REDACTED]"), "{text}");
    let log = std::fs::read_to_string(log_path_of(&text).unwrap()).unwrap();
    assert!(!log.contains(KEY));
}

#[test]
fn no_answer_counts_as_failure_and_falls_back_to_command() {
    let f = Fixture::new();
    let (_good, invocation) = good_script(f.dir.path());
    let config = format!(
        "{}\n[backends.good]\nharness = \"command\"\ndescription = \"good\"\n{invocation}",
        f.oc_backend(true, &rejected_lines(), 0)
    );
    let mut server = f.start(&config, &[]);

    let (is_error, text) = f.run(&mut server, json!({"prompt": "read .env"}));
    assert!(is_error, "{text}");
    assert!(text.contains("exit_code: 0"), "{text}");
    assert!(
        text.contains("failed: no answer: permission auto-rejected"),
        "{text}"
    );
    assert!(text.contains("stopped: permission auto-rejected"), "{text}");

    let (is_error, text) = f.run(
        &mut server,
        json!({"prompt": "read .env", "fallback": "good"}),
    );
    assert!(!is_error, "{text}");
    assert!(
        text.starts_with(
            "backend: good (command) — fallback after 'oc' failed: no answer: permission \
             auto-rejected (exit_code 0)\n"
        ),
        "{text}"
    );
    assert!(text.contains("GOOD-RAN"), "{text}");
}

#[test]
fn provider_error_is_reported() {
    let f = Fixture::new();
    let mut server = f.start(&f.oc_backend(true, &error_lines(), 1), &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(is_error, "{text}");
    assert!(text.contains("exit_code: 1"), "{text}");
    assert!(
        text.contains("[error] APIError: model does not support tools"),
        "{text}"
    );
}

/// More output than the runner's in-memory buffer (64-128 KiB): the usage line still counts
/// every step, because the result is computed from the complete output in the run log.
#[test]
fn usage_counts_all_steps_of_a_long_run() {
    const STEPS: usize = 300;
    let padding = "x".repeat(1000);
    let mut canned = String::new();
    for i in 0..STEPS {
        canned.push_str(&ev(
            json!({"type":"step_start","sessionID":"s1","part":{"type":"step-start"}}),
        ));
        canned.push('\n');
        canned.push_str(&ev(json!({"type":"tool_use","sessionID":"s1","part":{"tool":"read","state":{"status":"completed","output": format!("{i} {padding}")}}})));
        canned.push('\n');
        canned.push_str(&ev(json!({"type":"step_finish","sessionID":"s1","part":{"reason":"tool-calls","tokens":{"input":100,"output":1},"cost":0.0}})));
        canned.push('\n');
    }
    canned.push_str(&answer_lines(ANSWER));
    assert!(canned.len() > 300 * 1024, "must exceed the runner's buffer");

    let f = Fixture::new();
    let mut server = f.start(&f.oc_backend(true, &canned, 0), &[]);
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi"}));
    assert!(!is_error, "{text}");
    assert!(text.contains(ANSWER), "{text}");
    let steps = STEPS + 2;
    let input = STEPS * 100 + 1200 + 1300;
    let output = STEPS + 30 + 12;
    assert!(
        text.contains(&format!(
            "[usage] steps {steps}, tokens in {input} / out {output}"
        )),
        "{text}"
    );
}

#[test]
fn claude_falls_back_to_opencode_and_the_oauth_token_stays_behind() {
    let f = Fixture::new();
    let claude = failing_claude(f.dir.path());
    let config = format!(
        "[backends.sidekick]\nharness = \"claude\"\ndescription = \"claude account\"\n\
         default = true\nconfig_dir = '{}'\n\n{}",
        f.dir.path().join("claude-cfg").display(),
        f.oc_backend(false, &answer_lines(ANSWER), 0)
    );
    let claude_s = claude.to_string_lossy().into_owned();
    let mut server = f.start(
        &config,
        &[
            ("AGENT_SIDEKICK_TOKEN", OAUTH),
            ("AGENT_SIDEKICK_CLAUDE", claude_s.as_str()),
        ],
    );
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi", "fallback": "oc"}));
    assert!(!is_error, "{text}");
    assert!(
        text.starts_with(
            "backend: oc (opencode), model m1 — fallback after 'sidekick' failed: exit_code 3\n"
        ),
        "{text}"
    );
    assert!(text.contains(ANSWER), "{text}");
    let names = f.env_names();
    for gone in ["CLAUDE_CODE_OAUTH_TOKEN", "AGENT_SIDEKICK_TOKEN"] {
        assert!(!has_name(&names, gone), "{gone} reached opencode");
    }
    assert_no_secrets_below(&f.rec);
    assert!(!text.contains(OAUTH_SUFFIX));
}

#[test]
fn backends_and_status_report_opencode() {
    let f = Fixture::new();
    let missing = f.dir.path().join(if cfg!(windows) {
        "no-opencode.exe"
    } else {
        "no-opencode"
    });
    let config = format!(
        "{}\n[backends.gone]\nharness = \"opencode\"\nprovider = \"ollama\"\n\
         base_url = '{}'\nmodel = \"qwen3\"\ndescription = \"not installed\"\nharness_path = '{}'\n",
        f.oc_backend(true, &answer_lines(ANSWER), 0),
        f.api.base_url,
        missing.display()
    );
    let mut server = f.start(&config, &[]);
    let res = server.call_tool("sidekick_backends", json!({}), RUN_WITHIN);
    let report: Value = serde_json::from_str(&text_of(&res)).unwrap();
    let list = report["backends"].as_array().unwrap();
    let get = |n: &str| list.iter().find(|b| b["name"] == n).unwrap().clone();
    let oc = get("oc");
    assert_eq!(oc["type"], "opencode");
    assert_eq!(oc["cost"], "paid");
    assert_eq!(oc["models"], json!(["m1", "m2"]));
    assert_eq!(oc["default_model"], "m1");
    assert_eq!(oc["reachable"], true, "{oc}");
    assert!(oc["detail"].as_str().unwrap().contains("HTTP 200"), "{oc}");
    let gone = get("gone");
    assert_eq!(gone["reachable"], false, "{gone}");
    assert_eq!(gone["cost"], "free");
    assert!(
        gone["detail"]
            .as_str()
            .unwrap()
            .starts_with("opencode not installed"),
        "{gone}"
    );
    assert!(!text_of(&res).contains(KEY));

    let status = text_of(&server.call_tool("sidekick_status", json!({}), RUN_WITHIN));
    assert!(
        status.contains("  oc (opencode, default): running 0, queued 0, max_concurrent -, model m1, models [m1, m2]\n"),
        "{status}"
    );
    assert!(status.contains("provider key (oc): present"), "{status}");
    assert!(!status.contains(KEY));

    // A run on the missing harness fails before starting, with the install hint.
    let (is_error, text) = f.run(&mut server, json!({"prompt": "hi", "backend": "gone"}));
    assert!(is_error, "{text}");
    assert!(text.contains("OpenCode executable not found"), "{text}");
}
