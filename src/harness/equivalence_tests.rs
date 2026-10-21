//! Legacy `type = ...` configs and the new `harness = ...` configs must resolve to the same
//! backends and produce byte-identical [`Launch`]es (program, argv, environment, stdin,
//! secrets, log header).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backends::{
    build_launch, resolve_with, Backend, BackendKind, BackendType, Backends, Cost, Credential,
    ResolveDefaults,
};
use crate::config::Config;
use crate::runner::{Launch, RunRequest};
use crate::token::Token;

fn abs(rel: &str) -> PathBuf {
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    Path::new(root).join(rel)
}

fn defaults() -> ResolveDefaults {
    ResolveDefaults {
        home: Some(abs("home")),
        legacy_agent_config_dir: Some(abs("home").join(".claude-sidekick")),
        config_dir: Some(abs("config").join("agent-sidekick")),
    }
}

fn resolve_str(text: &str) -> Result<Backends, String> {
    let cfg: Config = toml::from_str(text).map_err(|e| format!("parse error: {e}"))?;
    resolve_with(&cfg, &defaults())
}

/// One backend set, with `{sidekick}`, `{work}`, `{local}`, `{remote}`, `{aider}`, `{stdin}`
/// standing for the kind line of each backend.
const TEMPLATE: &str = r#"
pass_env = ["CLAUDE_CODE_MAX_OUTPUT_TOKENS"]

[backends.sidekick]
{sidekick}
description = "Second Claude account"
default = true

[backends.work]
{work}
description = "Work account"
config_dir = "~/.claude-work"
model = "opus"
models = ["opus", "sonnet"]
max_concurrent = 1
env = { CLAUDE_CODE_ATTRIBUTION_HEADER = "0" }
pass_env = ["CLAUDE_CODE_GIT_BASH_PATH"]

[backends.local]
{local}
description = "Ollama"
base_url = "http://localhost:11434"
auth = "bearer"
model = "qwen3"
cost = "free"

[backends.remote]
{remote}
description = "Gateway with a key from the environment"
base_url = "https://gw.example.com/anthropic"
api_key_env = "GW_KEY"

[backends.aider]
{aider}
description = "Aider"
command = ["aider", "--model", "{model}", "--message", "{prompt}", "--yes-always"]
models = ["ollama_chat/qwen3", "ollama_chat/llama3"]
secret_env = "OPENAI_API_KEY"
env = { OLLAMA_API_BASE = "http://localhost:11434" }

[backends.stdin]
{stdin}
description = "Prompt via stdin"
command = ["agent", "--cwd", "{cwd}", "--turns", "{max_turns}"]
prompt_via_stdin = true
"#;

fn config(kinds: [&str; 6]) -> String {
    let names = ["sidekick", "work", "local", "remote", "aider", "stdin"];
    let mut text = TEMPLATE.to_owned();
    for (name, kind) in names.iter().zip(kinds) {
        text = text.replace(&format!("{{{name}}}"), kind);
    }
    text
}

fn legacy() -> String {
    config([
        r#"type = "claude-oauth""#,
        r#"type = "claude-oauth""#,
        r#"type = "claude-endpoint""#,
        r#"type = "claude-endpoint""#,
        r#"type = "command""#,
        r#"type = "command""#,
    ])
}

fn harness() -> String {
    config([
        r#"harness = "claude""#,
        r#"harness = "claude""#,
        r#"harness = "claude""#,
        r#"harness = "claude""#,
        r#"harness = "command""#,
        r#"harness = "command""#,
    ])
}

fn both() -> String {
    config([
        "harness = \"claude\"\ntype = \"claude-oauth\"",
        "type = \"claude-oauth\"\nharness = \"claude\"",
        "harness = \"claude\"\ntype = \"claude-endpoint\"",
        "type = \"claude-endpoint\"\nharness = \"claude\"",
        "harness = \"command\"\ntype = \"command\"",
        "type = \"command\"\nharness = \"command\"",
    ])
}

fn credential(b: &Backend) -> Credential {
    match &b.kind {
        BackendKind::ClaudeOauth { .. } => {
            Credential::OauthToken(Token::new(format!("sk-ant-oat01-token-of-{}", b.name)))
        }
        BackendKind::ClaudeEndpoint { .. } => {
            Credential::ApiKey(Token::new(format!("endpoint-key-of-{}-123", b.name)))
        }
        BackendKind::Command {
            secret_env: Some(_),
            ..
        } => Credential::CommandSecret(Token::new(format!("command-secret-of-{}", b.name))),
        BackendKind::Command {
            secret_env: None, ..
        } => Credential::None,
        BackendKind::OpenCode { .. } => {
            Credential::ProviderKey(Token::new(format!("provider-key-of-{}-123", b.name)))
        }
    }
}

fn program(b: &Backend) -> PathBuf {
    match &b.kind {
        BackendKind::Command { command, .. } => abs("bin").join(&command[0]),
        _ => abs("bin").join("claude"),
    }
}

/// A server environment inherited from a Claude Code session, with competing credentials.
fn base() -> Vec<(OsString, OsString)> {
    [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_SESSION_ID", "sess"),
        ("CLAUDE_CODE_ENTRYPOINT", "claude-vscode"),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000"),
        ("CLAUDE_CODE_GIT_BASH_PATH", "C:\\Git\\bin\\bash.exe"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-caller-token"),
        ("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
        ("ANTHROPIC_API_KEY", "caller-api-key"),
        ("ANTHROPIC_BASE_URL", "http://caller-proxy"),
        ("ANTHROPIC_CUSTOM_HEADERS", "x-gw: caller"),
        ("ANTHROPIC_MODEL", "caller-model"),
        ("AGENT_SIDEKICK_TOKEN", "sk-ant-oat01-legacy"),
        ("GW_KEY", "gateway-key-value-1"),
        ("OPENAI_API_KEY", "caller-openai"),
    ]
    .iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect()
}

fn request(model: Option<&str>) -> RunRequest {
    RunRequest {
        prompt: "-fix the {cwd} bug".to_owned(),
        cwd: abs("work"),
        max_turns: 7,
        model: model.map(str::to_owned),
        allowed_tools: Some(vec!["Read".to_owned()]),
        timeout: Duration::from_secs(60),
    }
}

fn launch(b: &Backend, model: Option<&str>) -> Launch {
    build_launch(
        b,
        &request(model),
        &credential(b),
        &program(b),
        base(),
        &["CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned()],
    )
    .unwrap_or_else(|e| panic!("build_launch {}: {e}", b.name))
}

fn assert_same_launch(a: &Launch, b: &Launch, what: &str) {
    assert_eq!(a.program, b.program, "{what}: program");
    assert_eq!(a.args, b.args, "{what}: args");
    assert_eq!(a.env, b.env, "{what}: env");
    assert_eq!(a.stdin, b.stdin, "{what}: stdin");
    assert!(a.secrets == b.secrets, "{what}: secrets");
    assert_eq!(a.header, b.header, "{what}: header");
}

#[test]
fn legacy_type_and_harness_configs_resolve_identically() {
    let legacy = resolve_str(&legacy()).unwrap();
    let harness = resolve_str(&harness()).unwrap();
    let both = resolve_str(&both()).unwrap();
    assert_eq!(legacy, harness);
    assert_eq!(legacy, both);

    let ty = |n: &str| harness.get(n).unwrap().backend_type();
    assert_eq!(ty("sidekick"), BackendType::ClaudeOauth);
    assert_eq!(ty("work"), BackendType::ClaudeOauth);
    assert_eq!(ty("local"), BackendType::ClaudeEndpoint);
    assert_eq!(ty("remote"), BackendType::ClaudeEndpoint);
    assert_eq!(ty("aider"), BackendType::Command);
    assert_eq!(ty("stdin"), BackendType::Command);
    // Type-dependent cost defaults follow the claude mode.
    assert_eq!(harness.get("sidekick").unwrap().cost, Cost::Quota);
    assert_eq!(harness.get("remote").unwrap().cost, Cost::Paid);
    assert_eq!(harness.get("stdin").unwrap().cost, Cost::Free);
    for b in harness.iter() {
        assert_eq!(b.harness_kind().as_str(), b.harness().name());
    }
}

#[test]
fn legacy_type_and_harness_configs_build_identical_launches() {
    let legacy = resolve_str(&legacy()).unwrap();
    let harness = resolve_str(&harness()).unwrap();
    for b in legacy.iter() {
        let h = harness.get(&b.name).unwrap();
        for model in [None, Some("sonnet"), Some("ollama_chat/llama3")] {
            if b.effective_model(model).is_err() {
                continue;
            }
            let what = format!("{} (model {model:?})", b.name);
            assert_same_launch(&launch(b, model), &launch(h, model), &what);
        }
    }
}

#[test]
fn harness_launches_keep_the_v0_3_shape() {
    let harness = resolve_str(&harness()).unwrap();
    let env_of = |l: &Launch, k: &str| {
        l.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    };

    // claude OAuth mode: the token, own config dir, session variables removed.
    let work = launch(harness.get("work").unwrap(), None);
    assert_eq!(
        env_of(&work, "CLAUDE_CODE_OAUTH_TOKEN").as_deref(),
        Some("sk-ant-oat01-token-of-work")
    );
    assert_eq!(
        env_of(&work, "CLAUDE_CONFIG_DIR").map(PathBuf::from),
        Some(abs("home").join(".claude-work"))
    );
    for gone in [
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "ANTHROPIC_API_KEY",
        "AGENT_SIDEKICK_TOKEN",
        "GW_KEY",
    ] {
        assert_eq!(env_of(&work, gone), None, "{gone}");
    }
    assert_eq!(
        env_of(&work, "CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
        Some("64000")
    );
    assert_eq!(work.header[0], "backend: work (claude-oauth), model opus");
    let args: Vec<String> = work
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(&args[..3], ["-p", "--max-turns", "7"]);
    assert!(args.ends_with(&["--".to_owned(), "-fix the {cwd} bug".to_owned()]));

    // claude endpoint mode: never an OAuth token, the key in the bearer variable.
    let local = launch(harness.get("local").unwrap(), None);
    assert_eq!(env_of(&local, "CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert_eq!(
        env_of(&local, "ANTHROPIC_AUTH_TOKEN").as_deref(),
        Some("endpoint-key-of-local-123")
    );
    assert_eq!(
        env_of(&local, "ANTHROPIC_BASE_URL").as_deref(),
        Some("http://localhost:11434")
    );
    assert_eq!(env_of(&local, "ANTHROPIC_MODEL"), None);

    // command: placeholders, secret_env, no claude credentials.
    let aider = launch(harness.get("aider").unwrap(), Some("ollama_chat/llama3"));
    let args: Vec<String> = aider
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--model",
            "ollama_chat/llama3",
            "--message",
            "-fix the {cwd} bug",
            "--yes-always"
        ]
    );
    assert_eq!(
        env_of(&aider, "OPENAI_API_KEY").as_deref(),
        Some("command-secret-of-aider")
    );
    for gone in ["CLAUDE_CODE_OAUTH_TOKEN", "AGENT_SIDEKICK_TOKEN", "GW_KEY"] {
        assert_eq!(env_of(&aider, gone), None, "{gone}");
    }
    let stdin = launch(harness.get("stdin").unwrap(), None);
    assert_eq!(stdin.stdin.as_deref(), Some("-fix the {cwd} bug"));
    assert_eq!(
        stdin.args,
        [
            OsString::from("--cwd"),
            abs("work").into_os_string(),
            OsString::from("--turns"),
            OsString::from("7")
        ]
    );
}

#[test]
fn an_oauth_token_never_reaches_another_harness() {
    let harness = resolve_str(&harness()).unwrap();
    let token = Credential::OauthToken(Token::new("sk-ant-oat01-should-not-leak"));
    for name in ["local", "remote", "aider", "stdin"] {
        let b = harness.get(name).unwrap();
        let e = build_launch(b, &request(None), &token, &program(b), base(), &[]).unwrap_err();
        assert!(e.contains("credential type does not match"), "{name}: {e}");
    }
}

#[test]
fn adapters_refuse_backends_of_another_harness() {
    let harness = resolve_str(&harness()).unwrap();
    let oauth = harness.get("work").unwrap();
    let cmd = harness.get("aider").unwrap();
    let no_env = |_: &str| None;
    let no_secret = |_: &crate::token::SecretId| Err(crate::token::TokenError::Missing);
    let e = super::HarnessKind::Command
        .adapter()
        .load_credential(oauth, &no_env, &no_secret)
        .unwrap_err();
    assert!(e.contains("internal error: harness command called for a claude-oauth backend"));
    let e = super::HarnessKind::Claude
        .adapter()
        .load_credential(cmd, &no_env, &no_secret)
        .unwrap_err();
    assert!(e.contains("internal error: harness claude called for a command backend"));
    let e = super::HarnessKind::Command
        .adapter()
        .locate(oauth, None)
        .unwrap_err();
    assert!(e.contains("internal error"), "{e}");
}

fn one(kind_lines: &str, extra: &str) -> String {
    format!("[backends.x]\n{kind_lines}\ndescription = \"d\"\ndefault = true\n{extra}")
}

#[test]
fn inconsistent_type_and_harness_is_a_config_error() {
    let cmd = "command = [\"x\", \"{prompt}\"]\n";
    for (kinds, extra, ty, h) in [
        (
            "type = \"claude-oauth\"\nharness = \"command\"",
            "",
            "claude-oauth",
            "command",
        ),
        (
            "type = \"claude-endpoint\"\nharness = \"command\"",
            "base_url = \"http://localhost:1\"\n",
            "claude-endpoint",
            "command",
        ),
        (
            "type = \"command\"\nharness = \"claude\"",
            cmd,
            "command",
            "claude",
        ),
    ] {
        let e = resolve_str(&one(kinds, extra)).unwrap_err();
        assert!(
            e.contains(&format!(
                "backends.x: `type = \"{ty}\"` and `harness = \"{h}\"` disagree"
            )),
            "{e}"
        );
    }

    // harness = "claude" with type claude-oauth but a base_url: the mode disagrees.
    let e = resolve_str(&one(
        "type = \"claude-oauth\"\nharness = \"claude\"",
        "base_url = \"http://localhost:1\"\n",
    ))
    .unwrap_err();
    assert!(
        e.contains("field `base_url` is not allowed for type `claude-oauth`"),
        "{e}"
    );
    // ... and claude-endpoint without one.
    let e = resolve_str(&one("type = \"claude-endpoint\"\nharness = \"claude\"", "")).unwrap_err();
    assert!(
        e.contains("`base_url` is required for type `claude-endpoint`"),
        "{e}"
    );
}

#[test]
fn harness_field_errors() {
    // Endpoint-only fields without base_url name the claude mode.
    let e = resolve_str(&one("harness = \"claude\"", "api_key_env = \"K\"\n")).unwrap_err();
    assert!(
        e.contains(
            "field `api_key_env` is not allowed for harness `claude` without `base_url` \
             (claude-oauth mode)"
        ),
        "{e}"
    );
    let e = resolve_str(&one(
        "harness = \"claude\"",
        "base_url = \"http://localhost:1\"\ncommand = [\"x\", \"{prompt}\"]\n",
    ))
    .unwrap_err();
    assert!(
        e.contains(
            "field `command` is not allowed for harness `claude` with `base_url` \
             (claude-endpoint mode)"
        ),
        "{e}"
    );
    let e = resolve_str(&one("harness = \"command\"", "base_url = \"http://x\"\n")).unwrap_err();
    assert!(
        e.contains("field `base_url` is not allowed for harness `command`"),
        "{e}"
    );
    // pass_env refusals name the harness as written; legacy `type` keeps the v0.3 text.
    let e = resolve_str(&one(
        "harness = \"claude\"",
        "pass_env = [\"CLAUDE_CODE_OAUTH_TOKEN\"]\n",
    ))
    .unwrap_err();
    assert!(
        e.contains(
            "pass_env: `CLAUDE_CODE_OAUTH_TOKEN` must not be passed through to a backend with \
             harness `claude` without `base_url` (claude-oauth mode)"
        ),
        "{e}"
    );
    let e = resolve_str(&one(
        "type = \"claude-oauth\"",
        "pass_env = [\"CLAUDE_CODE_OAUTH_TOKEN\"]\n",
    ))
    .unwrap_err();
    assert!(
        e.contains("must not be passed through to a claude-oauth backend"),
        "{e}"
    );
    // Neither harness nor type.
    let e = resolve_str(&one("", "")).unwrap_err();
    assert!(e.contains("`type` is required"), "{e}");
    assert!(
        e.contains("or `harness` (one of: claude, opencode, command)"),
        "{e}"
    );
    // Harnesses of later versions are not known yet.
    let e = resolve_str(&one("harness = \"codex\"", "")).unwrap_err();
    assert!(e.starts_with("parse error"), "{e}");
    // opencode has no legacy `type`.
    let e = resolve_str(&one("type = \"opencode\"", "")).unwrap_err();
    assert!(e.contains("`type = \"opencode\"` is not supported"), "{e}");
    let e = resolve_str(&one(
        "type = \"opencode\"
harness = \"opencode\"",
        "",
    ))
    .unwrap_err();
    assert!(e.contains("write `harness = \"opencode\"`"), "{e}");
}
