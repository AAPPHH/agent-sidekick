use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::backends::{resolve_with, BackendType, Backends, Cost, ResolveDefaults};
use crate::config::Config;
use crate::harness::RateLimit;
use crate::runner::RunRequest;
use crate::token::TokenSource;

const KEY: &str = "kic-test-provider-key-1234567890";
const OAUTH: &str = "sk-ant-oat01-oauth-token-of-the-caller-0123456789";

fn abs(rel: &str) -> PathBuf {
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    Path::new(root).join(rel)
}

fn defaults(config_dir: &Path) -> ResolveDefaults {
    ResolveDefaults {
        home: Some(abs("home")),
        legacy_agent_config_dir: Some(abs("home").join(".claude-sidekick")),
        config_dir: Some(config_dir.to_path_buf()),
    }
}

fn resolve_in(text: &str, config_dir: &Path) -> Result<Backends, String> {
    let cfg: Config = toml::from_str(text).map_err(|e| format!("parse error: {e}"))?;
    resolve_with(&cfg, &defaults(config_dir))
}

fn resolve_str(text: &str) -> Result<Backends, String> {
    resolve_in(text, &abs("cfg"))
}

/// The KI:connect example from docs/REFERENCE.md / examples/config.toml.
const KICONNECT: &str = r#"
[backends.kiconnect]
harness = "opencode"
provider = "openai-compatible"
base_url = "https://chat.kiconnect.nrw/api/v1/"
models = ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]
description = "KI:connect NRW via OpenCode"
default = true
"#;

fn one(body: &str) -> String {
    format!("[backends.x]\nharness = \"opencode\"\ndescription = \"d\"\ndefault = true\n{body}")
}

fn kiconnect(config_dir: &Path) -> Backend {
    resolve_in(KICONNECT, config_dir)
        .unwrap()
        .get("kiconnect")
        .unwrap()
        .clone()
}

fn req(cwd: &Path) -> RunRequest {
    RunRequest {
        prompt: "Say \"hi\" & list files".to_owned(),
        cwd: cwd.to_path_buf(),
        max_turns: 7,
        model: None,
        allowed_tools: None,
        timeout: Duration::from_secs(60),
    }
}

fn env_get<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsString> {
    env.iter().find(|(k, _)| eq_name(k, name)).map(|(_, v)| v)
}

fn poisoned_base(real_home: &Path) -> Vec<(OsString, OsString)> {
    [
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("CLAUDE_CODE_OAUTH_TOKEN", OAUTH.to_owned()),
        ("CLAUDECODE", "1".to_owned()),
        ("CLAUDE_CODE_SESSION_ID", "s".to_owned()),
        ("AGENT_SIDEKICK_TOKEN", OAUTH.to_owned()),
        (
            "ANTHROPIC_API_KEY",
            "sk-ant-api03-caller-key-0123456789".to_owned(),
        ),
        (
            "ANTHROPIC_AUTH_TOKEN",
            "caller-auth-token-0123456789".to_owned(),
        ),
        ("ANTHROPIC_BASE_URL", "http://evil.example".to_owned()),
        ("ANTHROPIC_MODEL", "x".to_owned()),
        ("OPENCODE_CONFIG", "/evil/opencode.json".to_owned()),
        (
            "OPENCODE_CONFIG_CONTENT",
            "{\"permission\":\"ask\"}".to_owned(),
        ),
        ("OPENCODE_CONFIG_DIR", "/evil".to_owned()),
        ("OPENCODE_PERMISSION", "{\"bash\":\"ask\"}".to_owned()),
        ("PWD", "/somewhere/else".to_owned()),
        ("OLDPWD", "/before".to_owned()),
        ("OPENAI_API_KEY", "sk-openai-caller-0123456789".to_owned()),
        ("OPENROUTER_API_KEY", "sk-or-caller-0123456789".to_owned()),
        (PROVIDER_KEY_ENV, "stale-provider-key-0123456789".to_owned()),
        ("NPM_CONFIG_CACHE", "/real/npm-cache".to_owned()),
        ("HOME", real_home.to_string_lossy().into_owned()),
        ("USERPROFILE", real_home.to_string_lossy().into_owned()),
        (
            "XDG_CONFIG_HOME",
            real_home.join(".config").to_string_lossy().into_owned(),
        ),
        ("TEMP", real_home.join("tmp").to_string_lossy().into_owned()),
        ("FOREIGN_KEY", "foreign-key-of-other-backend-123".to_owned()),
        ("CMD_SECRET", "secret-of-a-command-backend-123".to_owned()),
        ("KEEP_ME", "kept".to_owned()),
        ("HOMEDRIVE", "Q:".to_owned()),
        ("HOMEPATH", r"\Users\real".to_owned()),
        ("HOMESHARE", r"\\fileserver\real".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
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

// ---------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------

#[test]
fn kiconnect_example_resolves() {
    let b = kiconnect(&abs("cfg"));
    assert_eq!(b.backend_type(), BackendType::OpenCode);
    assert_eq!(b.harness_kind(), HarnessKind::OpenCode);
    assert_eq!(b.cost, Cost::Paid);
    assert_eq!(b.model.as_deref(), Some("openai-gpt-oss-120b"));
    assert_eq!(
        b.models,
        Some(vec![
            "openai-gpt-oss-120b".to_owned(),
            "qwen-qwen3-8-27b".to_owned()
        ])
    );
    let BackendKind::OpenCode {
        provider,
        base_url,
        api_key_env,
        harness_path,
        read_only,
        request_timeout_s,
        model_options,
        limits,
        base_dir,
    } = &b.kind
    else {
        panic!("{b:?}")
    };
    assert_eq!(*request_timeout_s, None);
    assert!(model_options.is_empty());
    assert_eq!(*limits, OpencodeLimits::default());
    assert_eq!(*provider, OpencodeProvider::OpenaiCompatible);
    assert_eq!(
        base_url.as_deref(),
        Some("https://chat.kiconnect.nrw/api/v1")
    );
    assert!(api_key_env.is_none() && harness_path.is_none() && !read_only);
    assert_eq!(*base_dir, abs("cfg").join("opencode").join("kiconnect"));
    assert_eq!(b.secret_id(), Some(SecretId::Backend("kiconnect".into())));
    assert_eq!(
        SecretId::Backend("kiconnect".into()).keyring_user(),
        "backend-kiconnect"
    );
    assert_eq!(
        SecretId::Backend("kiconnect".into()).file_name(),
        "backend-kiconnect.key"
    );
    assert!(b.takes_model());
    assert!(b.effective_model(Some("gpt-4")).is_err());
    assert_eq!(
        b.effective_model(Some("qwen-qwen3-8-27b"))
            .unwrap()
            .as_deref(),
        Some("qwen-qwen3-8-27b")
    );
}

#[test]
fn config_rules() {
    let err = |body: &str| resolve_str(&one(body)).unwrap_err();
    let has = |e: String, what: &str| assert!(e.contains(what), "{e}");
    has(
        err("models = [\"m\"]"),
        "`provider` is required for harness `opencode`",
    );
    has(
        err("provider = \"openai-compatible\"\nmodels = [\"m\"]"),
        "`base_url` is required for provider `openai-compatible`",
    );
    has(
        err("provider = \"ollama\"\nmodels = [\"m\"]"),
        "`base_url` is required for provider `ollama`",
    );
    has(
        err("provider = \"openrouter\"\nbase_url = \"https://x\"\nmodels = [\"m\"]"),
        "`base_url` is not allowed for provider `openrouter`",
    );
    has(
        err("provider = \"ollama\"\nbase_url = \"http://localhost:11434\"\napi_key_env = \"K\"\nmodels = [\"m\"]"),
        "`api_key_env` is not allowed for provider `ollama`",
    );
    has(
        err("provider = \"openrouter\""),
        "`models` (or `model`) is required for harness `opencode`",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\nharness_path = \"rel/opencode\""),
        "`harness_path` must be an absolute path",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\ncommand = [\"x\"]"),
        "field `command` is not allowed for harness `opencode`",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\nconfig_dir = \"/x\""),
        "field `config_dir` is not allowed for harness `opencode`",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\nauth = \"bearer\""),
        "field `auth` is not allowed for harness `opencode`",
    );
    has(
        err(
            "provider = \"openrouter\"\nmodels = [\"m\"]\npass_env = [\"CLAUDE_CODE_OAUTH_TOKEN\"]",
        ),
        "must not be passed through to a backend with harness `opencode`",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\npass_env = [\"AGENT_SIDEKICK_PROVIDER_KEY\"]"),
        "must not be passed through",
    );
    has(
        err("provider = \"openrouter\"\nmodels = [\"m\"]\napi_key_env = \"CLAUDE_CODE_OAUTH_TOKEN\""),
        "`api_key_env` must not be",
    );
    has(
        err("provider = \"bedrock\"\nmodels = [\"m\"]"),
        "parse error",
    );
    // Fields of opencode on other harnesses.
    let e = resolve_str(
        "[backends.c]\nharness = \"command\"\ndescription = \"d\"\ndefault = true\n\
         command = [\"x\", \"{prompt}\"]\nprovider = \"ollama\"\nread_only = true\n",
    )
    .unwrap_err();
    has(
        e.clone(),
        "field `provider` is not allowed for harness `command`",
    );
    has(e, "field `read_only` is not allowed for harness `command`");

    // A lone `model` is the allowlist; ollama is free by default, harness_path with `~`.
    let bs = resolve_str(&one(
        "provider = \"ollama\"\nbase_url = \"http://localhost:11434\"\nmodel = \"qwen3\"\n\
         harness_path = \"~/bin/opencode\"\nread_only = true",
    ))
    .unwrap();
    let b = bs.get("x").unwrap();
    assert_eq!(b.models, Some(vec!["qwen3".to_owned()]));
    assert_eq!(b.cost, Cost::Free);
    assert_eq!(b.secret_id(), None);
    let BackendKind::OpenCode {
        harness_path,
        read_only,
        ..
    } = &b.kind
    else {
        panic!()
    };
    assert_eq!(
        harness_path.as_deref(),
        Some(abs("home").join("bin/opencode").as_path())
    );
    assert!(*read_only);
    // api_key_env replaces the stored key.
    let bs = resolve_str(&one(
        "provider = \"openrouter\"\nmodels = [\"a/b\"]\napi_key_env = \"MY_OR_KEY\"",
    ))
    .unwrap();
    let b = bs.get("x").unwrap();
    assert_eq!(b.secret_id(), None);
    assert!(b.foreign_secret_env.contains(&"MY_OR_KEY".to_owned()));
}

#[test]
fn secret_env_and_api_key_env_are_foreign_to_every_backend() {
    let text = format!(
        "{KICONNECT}\n[backends.cmd]\nharness = \"command\"\ndescription = \"c\"\n\
         command = [\"x\", \"{{prompt}}\"]\nsecret_env = \"CMD_SECRET\"\n\
         [backends.ep]\nharness = \"claude\"\ndescription = \"e\"\nbase_url = \"http://localhost:1\"\n\
         api_key_env = \"FOREIGN_KEY\"\n"
    );
    let backends = resolve_str(&text).unwrap();
    for b in backends.iter() {
        assert!(
            b.foreign_secret_env.contains(&"CMD_SECRET".to_owned()),
            "{}",
            b.name
        );
        assert!(
            b.foreign_secret_env.contains(&"FOREIGN_KEY".to_owned()),
            "{}",
            b.name
        );
    }
}

// ---------------------------------------------------------------------------------------
// Generated config
// ---------------------------------------------------------------------------------------

#[test]
fn rendered_config_openai_compatible() {
    let b = kiconnect(&abs("cfg"));
    let c = render_config(&b).unwrap();
    assert_eq!(c["enabled_providers"], json!(["sidekick"]));
    assert_eq!(c["model"], "sidekick/openai-gpt-oss-120b");
    assert_eq!(c["small_model"], "sidekick/openai-gpt-oss-120b");
    assert_eq!(c["autoupdate"], false);
    assert_eq!(c["share"], "disabled");
    assert_eq!(c["snapshot"], false);
    assert_eq!(c["lsp"], false);
    assert_eq!(c["formatter"], false);
    assert_eq!(c["plugin"], json!([]));
    let p = &c["provider"]["sidekick"];
    assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(p["options"]["baseURL"], "https://chat.kiconnect.nrw/api/v1");
    assert_eq!(p["options"]["apiKey"], "{env:AGENT_SIDEKICK_PROVIDER_KEY}");
    // Unset `request_timeout_s`: OpenCode's own header timeout; `timeout` (a total request
    // limit without retry) is never set.
    assert!(p["options"].get("headerTimeout").is_none(), "{p}");
    assert!(p["options"].get("timeout").is_none(), "{p}");
    assert_eq!(
        p["models"]["qwen-qwen3-8-27b"],
        json!({"name": "qwen-qwen3-8-27b", "tool_call": true, "limit": {"context": 128000, "output": 8192}})
    );
    assert_eq!(
        p["models"].as_object().unwrap().len(),
        2,
        "every model of the allowlist"
    );
    assert_eq!(c["permission"], permissions(false));
    assert!(
        c.get("agent").is_none(),
        "per-run values are not in the file"
    );
}

#[test]
fn rendered_config_ollama_and_openrouter() {
    let bs = resolve_str(&one(
        "provider = \"ollama\"\nbase_url = \"http://localhost:11434/\"\nmodels = [\"qwen3:8b\"]",
    ))
    .unwrap();
    let c = render_config(bs.get("x").unwrap()).unwrap();
    let p = &c["provider"]["sidekick"];
    assert_eq!(p["options"]["baseURL"], "http://localhost:11434/v1");
    assert_eq!(p["options"]["apiKey"], "ollama");
    assert_eq!(c["model"], "sidekick/qwen3:8b");

    let bs = resolve_str(&one(
        "provider = \"openrouter\"\nmodels = [\"qwen/qwen3-coder\", \"openai/gpt-oss-120b\"]\n\
         model = \"openai/gpt-oss-120b\"\nread_only = true",
    ))
    .unwrap();
    let c = render_config(bs.get("x").unwrap()).unwrap();
    assert_eq!(c["enabled_providers"], json!(["openrouter"]));
    assert_eq!(c["model"], "openrouter/openai/gpt-oss-120b");
    let p = &c["provider"]["openrouter"];
    assert_eq!(p["options"]["apiKey"], "{env:AGENT_SIDEKICK_PROVIDER_KEY}");
    assert!(p.get("npm").is_none());
    assert_eq!(p["models"]["qwen/qwen3-coder"], json!({}));
    assert!(p["options"].get("headerTimeout").is_none(), "{p}");
    assert_eq!(c["permission"], permissions(true));
}

#[test]
fn model_options_are_merged_into_every_model() {
    let bs = resolve_str(&one("provider = \"openai-compatible\"
base_url = \"https://api.groq.com/openai/v1\"
         models = [\"a\", \"b\"]
         model_options = { interleaved = { field = \"reasoning\" }, limit = { output = 16384 } }
"))
    .unwrap();
    let c = render_config(bs.get("x").unwrap()).unwrap();
    for m in ["a", "b"] {
        assert_eq!(
            c["provider"]["sidekick"]["models"][m],
            json!({
                "name": m,
                "tool_call": true,
                "interleaved": {"field": "reasoning"},
                "limit": {"output": 16384}
            })
        );
    }
    let bs = resolve_str(&one("provider = \"ollama\"
base_url = \"http://localhost:11434\"
models = [\"m\"]
         [backends.x.model_options]
reasoning = true
temperature = 0.5
"))
    .unwrap();
    let c = render_config(bs.get("x").unwrap()).unwrap();
    assert_eq!(
        c["provider"]["sidekick"]["models"]["m"],
        json!({"name": "m", "tool_call": true, "reasoning": true, "temperature": 0.5, "limit": {"context": 128000, "output": 8192}})
    );
}

#[test]
fn model_options_config_errors() {
    const COMPAT: &str = "provider = \"openai-compatible\"
base_url = \"https://x\"
models = [\"m\"]
";
    let err = |body: &str| resolve_str(&one(body)).unwrap_err();
    let has = |e: String, what: &str| assert!(e.contains(what), "{e}");
    has(
        err("provider = \"openrouter\"
models = [\"m\"]
model_options = { a = 1 }"),
        "`model_options` is not allowed for provider `openrouter`",
    );
    for key in ["name", "tool_call"] {
        has(
            err(&format!("{COMPAT}model_options = {{ {key} = true }}")),
            &format!("`model_options` must not set `{key}`"),
        );
    }
    has(
        err(&format!(
            "{COMPAT}model_options = {{ a = {{ b = [1, nan] }} }}"
        )),
        "`model_options.a.b[1]`: not a JSON-compatible value",
    );
    has(
        err(&format!("{COMPAT}model_options = {{ when = 2026-10-22 }}")),
        "`model_options.when`: not a JSON-compatible value (TOML dates",
    );
    has(err(&format!("{COMPAT}model_options = 5")), "parse error");
    has(
        resolve_str(
            "[backends.x]
harness = \"command\"
description = \"d\"
             command = [\"x\", \"{prompt}\"]
model_options = { a = 1 }
",
        )
        .unwrap_err(),
        "field `model_options` is not allowed for harness `command`",
    );
}

#[test]
fn request_timeout_is_configurable_and_bounded() {
    const OLLAMA: &str =
        "provider = \"ollama\"\nbase_url = \"http://localhost:11434\"\nmodels = [\"m\"]\n";
    let bs = resolve_str(&one(&format!("{OLLAMA}request_timeout_s = 900"))).unwrap();
    let c = render_config(bs.get("x").unwrap()).unwrap();
    assert_eq!(
        c["provider"]["sidekick"]["options"]["headerTimeout"],
        900_000
    );
    for bad in ["0", "3601"] {
        let e = resolve_str(&one(&format!("{OLLAMA}request_timeout_s = {bad}"))).unwrap_err();
        assert!(
            e.contains("`request_timeout_s` must be between 1 and 3600"),
            "{e}"
        );
    }
    let e = resolve_str(
        "[backends.x]\ntype = \"command\"\ndescription = \"d\"\ncommand = [\"x\", \"{prompt}\"]\n\
         request_timeout_s = 60\n",
    )
    .unwrap_err();
    assert!(
        e.contains("field `request_timeout_s` is not allowed"),
        "{e}"
    );
}

#[test]
fn permission_sets_never_ask() {
    for ro in [false, true] {
        let p = permissions(ro);
        let text = p.to_string();
        assert!(!text.contains("\"ask\""), "{text}");
        for key in ["edit", "bash", "webfetch"] {
            assert_eq!(p[key], if ro { "deny" } else { "allow" });
        }
        for key in [
            "websearch",
            "task",
            "external_directory",
            "doom_loop",
            "question",
        ] {
            assert_eq!(p[key], "deny");
        }
        for key in ["glob", "grep", "list"] {
            assert_eq!(p[key], "allow");
        }
    }
    // "Last matching rule wins": the serialized order must keep `*` first and the
    // `.env.example` allowance after the `.env` denials.
    let text = permissions(false)["read"].to_string();
    assert_eq!(
        text,
        r#"{"*":"allow","*.env":"deny","*.env.*":"deny","*.env.example":"allow"}"#
    );
    assert_eq!(
        run_config(&kiconnect(Path::new("/cfg")), "m", 12),
        json!({
            "agent": {"build": {"steps": 12}},
            "compaction": {"auto": true, "prune": true},
            "tool_output": {"max_bytes": 32000},
            "provider": {"sidekick": {"models": {"m": {"limit": {"context": 128000, "output": 8192}}}}}
        })
    );
}

// ---------------------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------------------

struct Fixture {
    tmp: tempfile::TempDir,
    config_dir: PathBuf,
    real_home: PathBuf,
    cwd: PathBuf,
    program: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("agent-sidekick");
    let real_home = tmp.path().join("real-home");
    let cwd = tmp.path().join("work");
    for d in [&config_dir, &real_home, &cwd] {
        std::fs::create_dir_all(d).unwrap();
    }
    let program = tmp.path().join(if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    });
    std::fs::write(&program, b"").unwrap();
    Fixture {
        tmp,
        config_dir,
        real_home,
        cwd,
        program,
    }
}

fn launch_for(f: &Fixture, b: &Backend, cred: &Credential, rq: &RunRequest) -> Launch {
    crate::backends::build_launch(b, rq, cred, &f.program, poisoned_base(&f.real_home), &[])
        .unwrap()
}

#[test]
fn launch_argv_stdin_env_and_files() {
    let f = fixture();
    let mut b = kiconnect(&f.config_dir);
    // As if other backends used these as api_key_env / secret_env.
    b.foreign_secret_env
        .extend(["FOREIGN_KEY".to_owned(), "CMD_SECRET".to_owned()]);
    let before = files_below(f.tmp.path());
    let cred = Credential::ProviderKey(Token::new(KEY));
    let mut rq = req(&f.cwd);
    rq.model = Some("qwen-qwen3-8-27b".into());
    rq.allowed_tools = Some(vec!["Read".into()]);
    let l = launch_for(&f, &b, &cred, &rq);

    // argv: no prompt, model qualified with the provider id, title set.
    let args: Vec<String> = l
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "run",
            "--format",
            "json",
            "--model",
            "sidekick/qwen-qwen3-8-27b",
            "--title",
            "agent-sidekick"
        ]
    );
    assert_eq!(l.program, f.program);
    assert_eq!(l.stdin.as_deref(), Some(rq.prompt.as_str()));
    assert!(!l.secrets.redact(KEY).contains(KEY));

    let env = &l.env;
    let get = |n: &str| env_get(env, n).map(|v| v.to_string_lossy().into_owned());
    // The key only in its variable; the OAuth token and every competing credential gone.
    assert_eq!(get(PROVIDER_KEY_ENV).as_deref(), Some(KEY));
    for gone in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "AGENT_SIDEKICK_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "PWD",
        "OLDPWD",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "FOREIGN_KEY",
        "CMD_SECRET",
    ] {
        assert!(get(gone).is_none(), "{gone} must not reach opencode");
    }
    for (_, v) in env {
        let v = v.to_string_lossy();
        assert!(!v.contains(OAUTH), "OAuth token leaked");
        assert!(!v.contains("stale-provider-key"), "inherited key kept");
    }
    assert_eq!(get("KEEP_ME").as_deref(), Some("kept"));
    // Isolation variables all point into <config-dir>/opencode/kiconnect.
    let base = f.config_dir.join("opencode").join("kiconnect");
    let mut iso = vec![
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "TEMP",
        "TMP",
        "TMPDIR",
        "npm_config_cache",
    ];
    if cfg!(windows) {
        iso.extend(["USERPROFILE", "APPDATA", "LOCALAPPDATA"]);
    }
    for n in iso {
        let v = PathBuf::from(get(n).unwrap_or_else(|| panic!("{n} not set")));
        assert!(v.starts_with(&base), "{n} = {}", v.display());
        assert!(v.is_dir(), "{n} dir created: {}", v.display());
    }
    if cfg!(windows) {
        // HOMEDRIVE + HOMEPATH name the isolated home too; HOMESHARE is gone.
        let joined = PathBuf::from(format!(
            "{}{}",
            get("HOMEDRIVE").unwrap(),
            get("HOMEPATH").unwrap()
        ));
        assert_eq!(joined, base.join("home"));
        assert!(get("HOMESHARE").is_none());
    }
    assert_eq!(
        env.iter()
            .filter(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("npm_config_cache"))
            .count(),
        1,
        "the inherited NPM_CONFIG_CACHE is replaced"
    );
    for flag in DISABLE_FLAGS {
        assert_eq!(get(flag).as_deref(), Some("1"), "{flag}");
    }
    let perm: Value = serde_json::from_str(&get("OPENCODE_PERMISSION").unwrap()).unwrap();
    assert_eq!(perm, permissions(false));
    let content: Value = serde_json::from_str(&get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
    assert_eq!(content, run_config(&b, "qwen-qwen3-8-27b", 7));

    // Files: only the generated config, below the backend's dir; no key in it.
    let after = files_below(f.tmp.path());
    let new: Vec<&PathBuf> = after.difference(&before).collect();
    let rel_base = base.strip_prefix(f.tmp.path()).unwrap();
    assert!(
        new.iter().all(|p| p.starts_with(rel_base)),
        "files outside the backend dir: {new:?}"
    );
    let cfg_text = std::fs::read_to_string(Dirs::new(&base).config_file()).unwrap();
    assert!(!cfg_text.contains(KEY));
    let cfg: Value = serde_json::from_str(&cfg_text).unwrap();
    assert_eq!(cfg, render_config(&b).unwrap());
    assert!(files_below(&f.real_home).is_empty());

    // Header: no secrets, notes on allowed_tools and the soft step cap.
    let header = l.header.join("\n");
    assert!(header.contains("backend: kiconnect (opencode), model qwen-qwen3-8-27b"));
    assert!(header.contains("provider: openai-compatible (https://chat.kiconnect.nrw/api/v1)"));
    assert!(header.contains("allowed_tools ignored"));
    assert!(header.contains("soft cap"));
    assert!(!header.contains(KEY));
}

#[test]
fn config_file_is_not_rewritten_when_unchanged() {
    let f = fixture();
    let b = kiconnect(&f.config_dir);
    let cred = Credential::ProviderKey(Token::new(KEY));
    launch_for(&f, &b, &cred, &req(&f.cwd));
    let file = Dirs::new(&f.config_dir.join("opencode").join("kiconnect")).config_file();
    let t1 = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let mut rq = req(&f.cwd);
    rq.max_turns = 99;
    let l = launch_for(&f, &b, &cred, &rq);
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), t1);
    let content = env_get(&l.env, "OPENCODE_CONFIG_CONTENT").unwrap();
    assert!(content.to_string_lossy().contains("99"));
}

#[test]
fn ollama_launch_has_no_key_and_read_only_permissions() {
    let f = fixture();
    let bs = resolve_in(
        &one(
            "provider = \"ollama\"\nbase_url = \"http://localhost:11434\"\nmodels = [\"qwen3\"]\n\
             read_only = true",
        ),
        &f.config_dir,
    )
    .unwrap();
    let b = bs.get("x").unwrap();
    let l = launch_for(&f, b, &Credential::None, &req(&f.cwd));
    assert!(env_get(&l.env, PROVIDER_KEY_ENV).is_none());
    let perm: Value = serde_json::from_str(
        &env_get(&l.env, "OPENCODE_PERMISSION")
            .unwrap()
            .to_string_lossy(),
    )
    .unwrap();
    assert_eq!(perm, permissions(true));
    assert!(l.header.iter().any(|h| h.starts_with("read_only")));
}

#[test]
fn credentials_must_match_and_never_be_an_oauth_token() {
    let f = fixture();
    let b = kiconnect(&f.config_dir);
    for wrong in [
        Credential::OauthToken(Token::new(OAUTH)),
        Credential::ApiKey(Token::new(KEY)),
        Credential::CommandSecret(Token::new(KEY)),
        Credential::None,
    ] {
        let r =
            crate::backends::build_launch(&b, &req(&f.cwd), &wrong, &f.program, Vec::new(), &[]);
        assert!(r.unwrap_err().contains("credential type does not match"));
    }
    let ollama = resolve_in(
        &one("provider = \"ollama\"\nbase_url = \"http://h:1\"\nmodels = [\"m\"]"),
        &f.config_dir,
    )
    .unwrap();
    let r = crate::backends::build_launch(
        ollama.get("x").unwrap(),
        &req(&f.cwd),
        &Credential::ProviderKey(Token::new(KEY)),
        &f.program,
        Vec::new(),
        &[],
    );
    assert!(r.is_err());
    // The other adapters refuse an opencode backend.
    for h in [HarnessKind::Claude, HarnessKind::Command] {
        let e = h
            .adapter()
            .load_credential(&b, &|_| None, &|_| Err(TokenError::Missing))
            .unwrap_err();
        assert!(e.contains("internal error"), "{e}");
    }
}

#[test]
fn pass_env_cannot_reintroduce_opencode_settings_or_pwd() {
    let f = fixture();
    let bs = resolve_in(
        &format!(
            "{KICONNECT}pass_env = [\"OPENCODE_CONFIG\", \"PWD\", \"OPENAI_API_KEY\", \
             \"ANTHROPIC_BASE_URL\", \"FOREIGN_KEY\"]\n"
        ),
        &f.config_dir,
    )
    .unwrap();
    let mut b = bs.get("kiconnect").unwrap().clone();
    b.foreign_secret_env.push("FOREIGN_KEY".into());
    let l = launch_for(
        &f,
        &b,
        &Credential::ProviderKey(Token::new(KEY)),
        &req(&f.cwd),
    );
    assert!(env_get(&l.env, "OPENCODE_CONFIG").is_none());
    assert!(env_get(&l.env, "PWD").is_none());
    assert!(env_get(&l.env, "ANTHROPIC_BASE_URL").is_none());
    assert!(env_get(&l.env, "FOREIGN_KEY").is_none());
    assert!(
        env_get(&l.env, "OPENAI_API_KEY").is_some(),
        "explicitly passed"
    );
}

#[test]
fn batch_wrapper_needs_safe_model_names() {
    let f = fixture();
    let bs = resolve_in(
        &one("provider = \"openrouter\"\nmodels = [\"a/b\", \"x&calc\"]"),
        &f.config_dir,
    )
    .unwrap();
    let b = bs.get("x").unwrap();
    let wrapper = f.tmp.path().join("opencode.cmd");
    std::fs::write(&wrapper, b"").unwrap();
    let cred = Credential::ProviderKey(Token::new(KEY));
    let mut rq = req(&f.cwd);
    rq.model = Some("x&calc".into());
    let r = crate::backends::build_launch(b, &rq, &cred, &wrapper, Vec::new(), &[]);
    if cfg!(windows) {
        assert!(r.unwrap_err().contains(".cmd/.bat"));
    }
    rq.model = Some("a/b".into());
    assert!(crate::backends::build_launch(b, &rq, &cred, &wrapper, Vec::new(), &[]).is_ok());
}

// ---------------------------------------------------------------------------------------
// Credentials, lookup, reachability
// ---------------------------------------------------------------------------------------

#[test]
fn load_credential_sources() {
    let b = kiconnect(&abs("cfg"));
    let h = OpencodeHarness;
    let (c, src) = h
        .load_credential(&b, &|_| None, &|id| {
            assert_eq!(*id, SecretId::Backend("kiconnect".into()));
            Ok((Token::new(KEY), TokenSource::Keyring))
        })
        .unwrap();
    assert_eq!(c, Credential::ProviderKey(Token::new(KEY)));
    assert!(src.contains("keyring"));
    let e = h
        .load_credential(&b, &|_| None, &|_| Err(TokenError::Missing))
        .unwrap_err();
    assert!(e.contains("setup --backend kiconnect"), "{e}");

    let bs = resolve_str(&one(
        "provider = \"openrouter\"\nmodels = [\"m\"]\napi_key_env = \"OR_KEY\"",
    ))
    .unwrap();
    let b = bs.get("x").unwrap();
    let (c, src) = h
        .load_credential(b, &|n| (n == "OR_KEY").then(|| KEY.to_owned()), &|_| {
            panic!("no store")
        })
        .unwrap();
    assert_eq!(c, Credential::ProviderKey(Token::new(KEY)));
    assert_eq!(src, "environment variable OR_KEY");
    assert!(h
        .load_credential(b, &|_| None, &|_| panic!("no store"))
        .unwrap_err()
        .contains("OR_KEY"));
}

#[test]
fn find_opencode_lookup_order() {
    let files: std::cell::RefCell<Vec<PathBuf>> = Default::default();
    let is_file = |p: &Path| files.borrow().iter().any(|f| f == p);
    let npm = abs("npm-prefix");
    let other = abs("other");
    let path_var = std::env::join_paths([npm.clone(), other.clone()]).unwrap();
    let env = |k: &str| match k {
        "PATH" => Some(path_var.clone()),
        "APPDATA" => Some(abs("appdata").into_os_string()),
        "HOME" => Some(abs("home").into_os_string()),
        _ => None,
    };
    // harness_path wins, and must exist.
    let hp = abs("custom/opencode");
    assert!(find_opencode(Some(&hp), &env, &is_file)
        .unwrap_err()
        .contains("harness_path"));
    files.borrow_mut().push(hp.clone());
    assert_eq!(find_opencode(Some(&hp), &env, &is_file).unwrap(), hp);
    assert!(find_opencode(None, &env, &is_file)
        .unwrap_err()
        .contains("opencode not found"));
    if cfg!(windows) {
        // A .cmd shim alone is never used; its npm package's exe is.
        files.borrow_mut().push(npm.join("opencode.cmd"));
        assert!(find_opencode(None, &env, &is_file).is_err());
        let exe = npm
            .join("node_modules")
            .join("opencode-ai")
            .join("bin")
            .join("opencode.exe");
        files.borrow_mut().push(exe.clone());
        files.borrow_mut().push(other.join("opencode.exe"));
        assert_eq!(find_opencode(None, &env, &is_file).unwrap(), exe);
        // Without the shim: %APPDATA%\npm, then PATH.
        files
            .borrow_mut()
            .retain(|p| *p != npm.join("opencode.cmd"));
        assert_eq!(
            find_opencode(None, &env, &is_file).unwrap(),
            other.join("opencode.exe")
        );
        let appdata_exe = abs("appdata")
            .join("npm")
            .join("node_modules")
            .join("opencode-ai")
            .join("bin")
            .join("opencode.exe");
        files.borrow_mut().push(appdata_exe.clone());
        assert_eq!(find_opencode(None, &env, &is_file).unwrap(), appdata_exe);
    } else {
        let home_bin = abs("home").join(".opencode/bin/opencode");
        files.borrow_mut().push(home_bin.clone());
        assert_eq!(find_opencode(None, &env, &is_file).unwrap(), home_bin);
        files.borrow_mut().push(other.join("opencode"));
        assert_eq!(
            find_opencode(None, &env, &is_file).unwrap(),
            other.join("opencode")
        );
        // <prefix>/bin in PATH: the npm package's binary in <prefix>/lib/node_modules.
        let bin = abs("usr/local/bin");
        let path2 = std::env::join_paths([bin.clone()]).unwrap();
        let env2 = |k: &str| (k == "PATH").then(|| path2.clone());
        let npm_bin = abs("usr/local/lib/node_modules/opencode-ai/bin/opencode.exe");
        files.borrow_mut().push(npm_bin.clone());
        assert_eq!(find_opencode(None, &env2, &is_file).unwrap(), npm_bin);
    }
}

#[test]
fn git_bash_detection() {
    let files = [
        abs("Git").join("cmd").join("git.exe"),
        abs("Git").join("bin").join("bash.exe"),
    ];
    let is_file = |p: &Path| files.iter().any(|f| f == p);
    let path_var = std::env::join_paths([abs("x"), abs("Git").join("cmd")]).unwrap();
    let env = |k: &str| (k == "PATH").then(|| path_var.clone());
    assert_eq!(git_bash(&env, &is_file), Some(files[1].clone()));
    let env2 = |k: &str| match k {
        "CLAUDE_CODE_GIT_BASH_PATH" => Some(files[1].clone().into_os_string()),
        _ => None,
    };
    assert_eq!(git_bash(&env2, &is_file), Some(files[1].clone()));
    assert_eq!(git_bash(&|_| None, &is_file), None);
}

fn deps_for<'a>(
    locate: &'a dyn Fn(&Backend) -> Result<PathBuf, String>,
    secret: &'a dyn Fn(&SecretId) -> Result<String, TokenError>,
    secret_value: &'a dyn Fn(&SecretId) -> Result<Token, TokenError>,
    probe_get: &'a dyn Fn(&str, Option<&str>) -> ProbeResponse,
) -> CheckDeps<'a> {
    CheckDeps {
        find_claude: &|| panic!("claude is not needed"),
        secret,
        env_lookup: &|_| None,
        resolve_program: &|_| panic!("no command"),
        probe: &|_| panic!("no unauthenticated base_url probe"),
        locate,
        secret_value,
        probe_get,
    }
}

#[test]
fn reachability_checks() {
    let b = kiconnect(&abs("cfg"));
    let h = OpencodeHarness;
    let found = |_: &Backend| Ok(abs("bin/opencode"));
    let stored = |_: &SecretId| Ok("OS keyring".to_owned());
    let value = |_: &SecretId| Ok(Token::new(KEY));
    let seen: std::cell::RefCell<Vec<(String, Option<String>)>> = Default::default();
    let ok200 = |u: &str, bearer: Option<&str>| {
        seen.borrow_mut()
            .push((u.to_owned(), bearer.map(str::to_owned)));
        ProbeResponse::Status {
            code: 200,
            url: u.to_owned(),
            millis: 3,
        }
    };
    let r = h.check(&b, &deps_for(&found, &stored, &value, &ok200));
    assert!(r.reachable, "{r:?}");
    assert!(r.detail.contains("key from OS keyring"), "{r:?}");
    assert!(!r.detail.contains(KEY));
    assert_eq!(
        seen.borrow().last().unwrap(),
        &(
            "https://chat.kiconnect.nrw/api/v1/models".to_owned(),
            Some(KEY.to_owned())
        )
    );

    // Not installed comes first.
    let missing = |_: &Backend| Err("opencode not found (...)".to_owned());
    let r = h.check(&b, &deps_for(&missing, &stored, &value, &ok200));
    assert!(
        !r.reachable && r.detail.starts_with("opencode not installed"),
        "{r:?}"
    );
    // No key.
    let none = |_: &SecretId| Err(TokenError::Missing);
    let r = h.check(&b, &deps_for(&found, &none, &value, &ok200));
    assert!(
        !r.reachable && r.detail.contains("no provider key stored"),
        "{r:?}"
    );
    // Key rejected.
    let denied = |u: &str, _: Option<&str>| ProbeResponse::Status {
        code: 401,
        url: u.to_owned(),
        millis: 3,
    };
    let r = h.check(&b, &deps_for(&found, &stored, &value, &denied));
    assert!(
        !r.reachable && r.detail.contains("rejected the key"),
        "{r:?}"
    );
    // Down.
    let down = |_: &str, _: Option<&str>| ProbeResponse::Failed("connection refused".into());
    let r = h.check(&b, &deps_for(&found, &stored, &value, &down));
    assert!(
        !r.reachable && r.detail.contains("unreachable: connection refused"),
        "{r:?}"
    );
    // A gateway without a model list still counts as reachable.
    let nf = |u: &str, _: Option<&str>| ProbeResponse::Status {
        code: 404,
        url: u.to_owned(),
        millis: 3,
    };
    assert!(
        h.check(&b, &deps_for(&found, &stored, &value, &nf))
            .reachable
    );

    // ollama: /api/tags, no key, no keyring access.
    let bs = resolve_str(&one(
        "provider = \"ollama\"\nbase_url = \"http://localhost:11434\"\nmodels = [\"m\"]",
    ))
    .unwrap();
    let no_store = |_: &SecretId| -> Result<String, TokenError> { panic!("no key for ollama") };
    let r = h.check(
        bs.get("x").unwrap(),
        &deps_for(&found, &no_store, &value, &ok200),
    );
    assert!(r.reachable, "{r:?}");
    assert_eq!(
        seen.borrow().last().unwrap(),
        &("http://localhost:11434/api/tags".to_owned(), None)
    );
    // openrouter: public model list, the key is not sent.
    let bs = resolve_str(&one("provider = \"openrouter\"\nmodels = [\"m\"]")).unwrap();
    assert!(
        h.check(
            bs.get("x").unwrap(),
            &deps_for(&found, &stored, &value, &ok200)
        )
        .reachable
    );
    assert_eq!(
        seen.borrow().last().unwrap(),
        &(OPENROUTER_MODELS_URL.to_owned(), None)
    );
}

// ---------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------

fn ev(v: Value) -> String {
    v.to_string()
}

#[test]
fn normalizes_text_after_last_step_and_usage() {
    let raw = [
        ev(json!({"type":"step_start","sessionID":"s","part":{"type":"step-start"}})),
        ev(json!({"type":"text","sessionID":"s","part":{"type":"text","text":"Let me look."}})),
        ev(json!({"type":"tool_use","sessionID":"s","part":{"tool":"read","state":{"status":"completed","output":"..."}}})),
        ev(json!({"type":"step_finish","sessionID":"s","part":{"reason":"tool-calls","tokens":{"input":100,"output":20},"cost":0.001}})),
        ev(json!({"type":"step_start","sessionID":"s","part":{"type":"step-start"}})),
        ev(json!({"type":"text","sessionID":"s","part":{"type":"text","text":"  The answer is 42.\n"}})),
        ev(json!({"type":"step_finish","sessionID":"s","part":{"reason":"stop","tokens":{"input":150,"output":10},"cost":0.002}})),
    ]
    .join("\n");
    let n = normalize_json_lines(&raw);
    assert_eq!(
        n.text,
        "The answer is 42.\n[usage] steps 2, tokens in 250 / out 30, cost 0.0030"
    );
    assert_eq!(n.failure, None);
}

#[test]
fn rate_limit_without_answer_is_a_failure() {
    let raw = ev(
        json!({"type":"error","sessionID":"s","error":{"name":"APIError","data":{"message":"Rate limit reached for model `m1` on tokens per minute (TPM): Limit 8000, Used 7000, Requested 4000. Please try again in 7.66s.","statusCode":429}}}),
    );
    let n = normalize_json_lines(&raw);
    assert!(n.failure.is_some());
    assert_eq!(
        n.rate_limit,
        Some(RateLimit {
            retry_after_s: Some(8)
        })
    );
    assert!(
        n.text.ends_with("\nrate_limited: yes (retry after 8s)"),
        "{}",
        n.text
    );

    let with_answer = format!(
        "{raw}\n{}",
        ev(json!({"type":"text","sessionID":"s","part":{"type":"text","text":"done"}}))
    );
    let n = normalize_json_lines(&with_answer);
    assert_eq!(n.failure, None);
    assert_eq!(n.rate_limit, None);
}

#[test]
fn rate_limit_text_in_tool_output_is_not_a_rate_limit() {
    let raw = [
        ev(json!({"type":"tool_use","sessionID":"s","part":{"tool":"bash","state":{"status":"completed","output":"grep: rate_limit_error 429 Too Many Requests"}}})),
        ev(json!({"type":"error","sessionID":"s","error":{"name":"APIError","data":{"message":"Internal server error","statusCode":500}}})),
    ]
    .join("\n");
    let n = normalize_json_lines(&raw);
    assert!(n.failure.is_some());
    assert_eq!(n.rate_limit, None);
    assert!(!n.text.contains("rate_limited"), "{}", n.text);
}

#[test]
fn normalizes_errors_rejections_and_noise() {
    // An error event, a cut-off first line, plain stderr and agent-sidekick's own note.
    let raw = [
        "on\":\"s\",\"part\":{\"type\":\"text\",\"text\":\"cut\"}}".to_owned(),
        ev(json!({"type":"error","sessionID":"s","error":{"name":"APIError","data":{"message":"\"auto\" tool choice requires --enable-auto-tool-choice","statusCode":400}}})),
        "Error: something on stderr".to_owned(),
        "[agent-sidekick] timed out after 60s, process tree killed".to_owned(),
    ]
    .join("\r\n");
    let n = normalize_json_lines(&raw);
    assert_eq!(
        n.text,
        "[opencode output]\nError: something on stderr\n[agent-sidekick] timed out after 60s, \
         process tree killed\n[error] APIError: \"auto\" tool choice requires \
         --enable-auto-tool-choice"
    );
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: opencode reported an error")
    );

    // Permission auto-rejected: exit 0, no text.
    let raw = [
        ev(json!({"type":"step_start","sessionID":"s","part":{}})),
        ev(json!({"type":"tool_use","sessionID":"s","part":{"tool":"read","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}})),
        "! permission requested: read (/x/.env); auto-rejecting".to_owned(),
    ]
    .join("\n");
    let n = normalize_json_lines(&raw);
    assert!(
        n.text.contains("stopped: permission auto-rejected"),
        "{}",
        n.text
    );
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: permission auto-rejected")
    );

    // Nothing at all.
    let n = normalize_json_lines("");
    assert_eq!(n.text, "");
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: no opencode JSON output")
    );
    // Events, but no text.
    let n = normalize_json_lines(&ev(json!({"type":"step_start","sessionID":"s","part":{}})));
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: opencode produced no text")
    );
    // Text only before the last step_start: all text is used as a fallback.
    let raw = [
        ev(json!({"type":"text","sessionID":"s","part":{"text":"partial"}})),
        ev(json!({"type":"step_start","sessionID":"s","part":{}})),
    ]
    .join("\n");
    let n = normalize_json_lines(&raw);
    assert_eq!(n.text, "partial");
    assert_eq!(n.failure, None);
}

#[test]
fn normalized_text_is_redacted_again_by_the_launch_secrets() {
    // A JSON-escaped key is not caught by line redaction of the raw output; the server
    // redacts the decoded text again (simulated here with the launch's Secrets).
    let key = "abc\"def-provider-key-123456";
    let raw = ev(json!({"type":"text","sessionID":"s","part":{"text": format!("key is {key}")}}));
    let secrets = Secrets::new([key]);
    assert!(
        secrets.redact(&raw).contains("abc\\\"def"),
        "raw escape not caught"
    );
    let n = normalize_json_lines(&secrets.redact(&raw));
    assert!(!secrets.redact(&n.text).contains(key));
}

#[test]
fn project_plugins_are_detected_up_to_the_repo_root() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("outer");
    let repo = outer.join("repo");
    let sub = repo.join("src").join("deep");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert!(project_plugin_sources(&sub).is_empty());
    // Above the repository root: not OpenCode's project, ignored.
    std::fs::create_dir_all(outer.join(".opencode").join("plugin")).unwrap();
    std::fs::write(outer.join(".opencode").join("plugin").join("x.js"), "").unwrap();
    std::fs::write(outer.join("opencode.json"), r#"{"plugin":["evil"]}"#).unwrap();
    assert!(project_plugin_sources(&sub).is_empty());
    // Inside: found; a directory without code files is not.
    std::fs::create_dir_all(repo.join(".opencode").join("plugins")).unwrap();
    std::fs::write(repo.join(".opencode").join("plugins").join("readme.md"), "").unwrap();
    assert!(project_plugin_sources(&sub).is_empty());
    std::fs::write(repo.join(".opencode").join("plugins").join("p.TS"), "").unwrap();
    assert_eq!(
        project_plugin_sources(&sub),
        vec![repo.join(".opencode").join("plugins")]
    );
    std::fs::remove_file(repo.join(".opencode").join("plugins").join("p.TS")).unwrap();

    // Config documents: no or an empty plugin list does not count.
    let root_doc = repo.join("opencode.json");
    std::fs::write(&root_doc, r#"{"model":"evil/evil","plugin":[]}"#).unwrap();
    std::fs::write(sub.join("opencode.json"), r#"{"plugins":null}"#).unwrap();
    assert!(project_plugin_sources(&sub).is_empty());
    // A plugin named in the root config (file URL or npm package) counts.
    std::fs::write(
        &root_doc,
        r#"{"plugin": ["file:///abs/.opencode/x/p.js", "some-npm-plugin"]}"#,
    )
    .unwrap();
    assert_eq!(project_plugin_sources(&sub), vec![root_doc.clone()]);
    // JSONC (comments, trailing comma) in .opencode/ of a subdirectory: textual match.
    let jsonc = sub.join(".opencode").join("opencode.jsonc");
    std::fs::create_dir_all(jsonc.parent().unwrap()).unwrap();
    std::fs::write(
        &jsonc,
        "{\n  // plugins of this package\n  \"plugins\" : [\"x\"],\n}\n",
    )
    .unwrap();
    assert_eq!(project_plugin_sources(&sub), vec![jsonc, root_doc]);
    assert!(!names_plugins(r#"["plugin"]"#));
    assert!(names_plugins(r#"{"plugin": "single"}"#));

    let f = fixture();
    let b = kiconnect(&f.config_dir);
    let mut rq = req(&sub);
    rq.cwd = sub.clone();
    let l = launch_for(&f, &b, &Credential::ProviderKey(Token::new(KEY)), &rq);
    let warning = l
        .header
        .iter()
        .find(|h| h.starts_with("warning: OpenCode runs the project plugins"))
        .unwrap_or_else(|| panic!("{:?}", l.header));
    assert!(warning.contains("opencode.jsonc"), "{warning}");
    assert!(warning.contains(PROVIDER_KEY_ENV), "{warning}");
    assert!(!warning.contains(KEY));
}

/// Two opencode backends never share state: own directories, generated configs, keys and
/// isolation variables, and a run of one leaves the other's directory untouched.
#[test]
fn two_opencode_backends_are_isolated_from_each_other() {
    let f = fixture();
    let cfg = r#"
[backends.alpha]
harness = "opencode"
provider = "openai-compatible"
base_url = "https://alpha.example/v1"
models = ["alpha-model"]
description = "a"
default = true

[backends.beta]
harness = "opencode"
provider = "openai-compatible"
base_url = "https://beta.example/v1"
models = ["beta-model"]
api_key_env = "BETA_KEY_VAR"
description = "b"
"#;
    let backends = resolve_in(cfg, &f.config_dir).unwrap();
    let a = backends.get("alpha").unwrap().clone();
    let b = backends.get("beta").unwrap().clone();
    let base_of = |be: &Backend| match &be.kind {
        BackendKind::OpenCode { base_dir, .. } => base_dir.clone(),
        other => panic!("{other:?}"),
    };
    let (base_a, base_b) = (base_of(&a), base_of(&b));
    assert_ne!(base_a, base_b);
    assert!(!base_a.starts_with(&base_b) && !base_b.starts_with(&base_a));
    assert!(a.foreign_secret_env.iter().any(|v| v == "BETA_KEY_VAR"));

    const KEY_A: &str = "alpha-provider-key-0123456789";
    const KEY_B: &str = "beta-provider-key-0123456789";
    let launch = |be: &Backend, key: &str| {
        let mut base = poisoned_base(&f.real_home);
        base.push(("BETA_KEY_VAR".into(), KEY_B.into()));
        crate::backends::build_launch(
            be,
            &req(&f.cwd),
            &Credential::ProviderKey(Token::new(key)),
            &f.program,
            base,
            &[],
        )
        .unwrap()
    };
    let snapshot = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
        files_below(dir)
            .into_iter()
            .map(|p| {
                let bytes = std::fs::read(dir.join(&p)).unwrap();
                (p, bytes)
            })
            .collect()
    };

    let la = launch(&a, KEY_A);
    let snap_a = snapshot(&base_a);
    assert!(!snap_a.is_empty());
    assert!(!base_b.exists(), "launching alpha created beta's directory");
    let lb = launch(&b, KEY_B);
    assert_eq!(
        snapshot(&base_a),
        snap_a,
        "launching beta changed alpha's files"
    );
    let snap_b = snapshot(&base_b);
    launch(&a, KEY_A);
    assert_eq!(
        snapshot(&base_b),
        snap_b,
        "launching alpha changed beta's files"
    );

    for (l, own_base, own_key, other_key, own_url, other_url) in [
        (&la, &base_a, KEY_A, KEY_B, "alpha.example", "beta.example"),
        (&lb, &base_b, KEY_B, KEY_A, "beta.example", "alpha.example"),
    ] {
        let get = |n: &str| env_get(&l.env, n).map(|v| v.to_string_lossy().into_owned());
        assert_eq!(get(PROVIDER_KEY_ENV).as_deref(), Some(own_key));
        for (_, v) in &l.env {
            assert!(!v.to_string_lossy().contains(other_key), "other key leaked");
        }
        assert!(
            get("BETA_KEY_VAR").is_none(),
            "api_key_env variable must not reach opencode"
        );
        for n in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "TEMP",
        ] {
            let v = PathBuf::from(get(n).unwrap());
            assert!(v.starts_with(own_base), "{n} = {}", v.display());
        }
        let text = std::fs::read_to_string(Dirs::new(own_base).config_file()).unwrap();
        assert!(
            text.contains(own_url) && !text.contains(other_url),
            "{text}"
        );
        assert!(!text.contains(own_key) && !text.contains(other_key));
    }
}

/// The opencode examples in docs/REFERENCE.md and (commented out) in examples/config.toml resolve.
#[test]
fn documented_examples_are_valid() {
    let readme = include_str!("../../../docs/REFERENCE.md").replace("\r\n", "\n");
    let marker = "```toml\n[backends.kiconnect]\nharness = \"opencode\"";
    let start = readme.find(marker).unwrap() + "```toml\n".len();
    let end = start + readme[start..].find("```").unwrap();
    let bs = resolve_str(&format!("{}default = true\n", &readme[start..end])).unwrap();
    let b = bs.get("kiconnect").unwrap();
    assert_eq!(b.backend_type(), BackendType::OpenCode);
    assert_eq!(b.models.as_ref().map(Vec::len), Some(2));
    assert_eq!(b.secret_id(), Some(SecretId::Backend("kiconnect".into())));

    let example = include_str!("../../../examples/config.toml").replace("\r\n", "\n");
    let start = example.find("# [backends.kiconnect-oc]").unwrap();
    let section: String = example[start..]
        .lines()
        .take_while(|l| !l.contains("read_only"))
        .map(|l| format!("{}\n", l.trim_start_matches("# ")))
        .collect();
    let bs = resolve_str(&format!("{section}default = true\n")).unwrap();
    let b = bs.get("kiconnect-oc").unwrap();
    assert_eq!(b.cost, Cost::Free);
    let BackendKind::OpenCode { base_url, .. } = &b.kind else {
        panic!()
    };
    assert_eq!(
        base_url.as_deref(),
        Some("https://chat.kiconnect.nrw/api/v1")
    );
}

#[cfg(windows)]
#[test]
fn split_drive_for_homedrive_and_homepath() {
    let s = |p: &str| {
        split_drive(Path::new(p)).map(|(d, r)| {
            (
                d.to_string_lossy().into_owned(),
                r.to_string_lossy().into_owned(),
            )
        })
    };
    assert_eq!(
        s(r"C:\Users\me\AppData\Roaming\agent-sidekick\opencode\x\home"),
        Some((
            "C:".into(),
            r"\Users\me\AppData\Roaming\agent-sidekick\opencode\x\home".into()
        ))
    );
    assert_eq!(
        s(r"\\?\D:\cfg\home"),
        Some(("D:".into(), r"\cfg\home".into()))
    );
    assert_eq!(
        s(r"\\srv\share\cfg\home"),
        Some((r"\\srv\share".into(), r"\cfg\home".into()))
    );
    assert_eq!(s(r"C:\"), Some(("C:".into(), r"\".into())));
    assert_eq!(s(r"relative\home"), None);
}
