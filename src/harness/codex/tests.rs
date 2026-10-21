use super::*;
use crate::backends::{ResolveDefaults, WindowsSandbox};
use crate::runner::RunRequest;
use crate::token::{TokenError, TokenSource};
use std::cell::RefCell;
use std::time::Duration;

const KEY: &str = "sk-proj-codex-test-key-0123456789";

fn env_of(vars: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
    move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
}

fn defaults(dir: &Path) -> ResolveDefaults {
    ResolveDefaults {
        home: Some(dir.join("home")),
        legacy_agent_config_dir: Some(dir.join("home").join(".claude-sidekick")),
        config_dir: Some(dir.join("cfg")),
    }
}

/// `[backends.gpt]` with `harness = "codex"` and `body`, resolved against `dir`.
fn resolve_codex(dir: &Path, body: &str) -> Result<Backend, String> {
    let text = format!(
        "[backends.gpt]\nharness = \"codex\"\ndescription = \"codex\"\ndefault = true\n{body}"
    );
    let cfg: crate::config::Config =
        toml::from_str(&text).map_err(|e| format!("parse error: {e}"))?;
    crate::backends::resolve_with(&cfg, &defaults(dir)).map(|b| b.get("gpt").unwrap().clone())
}

fn req(cwd: &Path) -> RunRequest {
    RunRequest {
        prompt: "Fix \"it\" & test".into(),
        cwd: cwd.to_path_buf(),
        max_turns: 7,
        model: None,
        allowed_tools: Some(vec!["Read".into()]),
        timeout: Duration::from_secs(30),
    }
}

fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn get<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(k, _)| eq_name(k, name))
        .and_then(|(_, v)| v.to_str())
}

/// The environment of an MCP server started inside a Codex session that was itself started
/// from a Claude Code terminal: every session and credential variable a child must not see.
fn poisoned_base() -> Vec<(OsString, OsString)> {
    os(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-caller-token"),
        ("CLAUDE_CODE_SESSION_ID", "s"),
        ("ANTHROPIC_API_KEY", "caller-api-key-123456"),
        ("CODEX_HOME", "/home/u/.codex"),
        ("CODEX_SQLITE_HOME", "/home/u/.codex"),
        ("CODEX_SESSION_ID", "sess"),
        ("CODEX_THREAD_ID", "thread"),
        ("CODEX_TOOL_CALL_ID", "call"),
        ("CODEX_CI", "1"),
        ("CODEX_VERSION", "0.163.0"),
        ("CODEX_SANDBOX", "seatbelt"),
        ("CODEX_SANDBOX_NETWORK_DISABLED", "1"),
        ("CODEX_MANAGED_BY_NPM", "1"),
        ("CODEX_API_KEY", "sk-caller-codex-key-123456"),
        ("CODEX_ACCESS_TOKEN", "caller-access-token-123456"),
        ("CODEX_CA_CERTIFICATE", "/etc/corp-ca.pem"),
        ("OPENAI_API_KEY", "sk-caller-openai-key-123456"),
        ("OPENAI_BASE_URL", "https://evil.example/v1"),
        ("RUST_LOG", "trace"),
        ("AGENT_SIDEKICK_PROVIDER_KEY", "inherited-provider-key-123"),
        ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
    ])
}

fn launch(
    b: &Backend,
    cred: &Credential,
    program: &Path,
    r: &RunRequest,
    model: Option<&str>,
) -> Result<Launch, String> {
    CodexHarness.build_launch(LaunchInput {
        backend: b,
        req: r,
        credential: cred,
        program,
        base: poisoned_base(),
        global_pass_env: &[],
        model: model.map(str::to_owned),
        header: vec!["backend: gpt (codex)".into()],
    })
}

fn strs(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

// ----- lookup and version -----

#[cfg(windows)]
#[test]
fn find_codex_windows_order() {
    let tmp = tempfile::tempdir().unwrap();
    let appdata = tmp.path().join("AppData").join("Roaming");
    let local = tmp.path().join("AppData").join("Local");
    let npm_default = appdata.join("npm");
    let custom = tmp.path().join("custom-npm");
    let plain = tmp.path().join("plain");
    let path = std::env::join_paths([&plain, &custom, &PathBuf::from("relative")]).unwrap();
    let env = env_of(vec![
        ("PATH", path),
        ("APPDATA", appdata.clone().into_os_string()),
        ("LOCALAPPDATA", local.clone().into_os_string()),
    ]);
    let files: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    let is_file = |p: &Path| files.borrow().iter().any(|f| f == p);

    // Nothing installed.
    let e = find_codex(None, &env, &is_file).unwrap_err();
    assert!(e.contains("codex not found"), "{e}");
    assert!(e.contains("harness install codex"), "{e}");

    // A shim alone is never used, neither the .cmd nor the .ps1.
    files.borrow_mut().push(custom.join("codex.cmd"));
    files.borrow_mut().push(custom.join("codex.ps1"));
    files.borrow_mut().push(npm_default.join("codex.cmd"));
    assert!(find_codex(None, &env, &is_file).is_err());

    // A codex.exe in PATH is the last resort.
    files.borrow_mut().push(plain.join("codex.exe"));
    assert_eq!(
        find_codex(None, &env, &is_file).unwrap(),
        plain.join("codex.exe")
    );

    // The standalone installer's location beats PATH.
    let standalone = local
        .join("Programs")
        .join("OpenAI")
        .join("Codex")
        .join("bin")
        .join("codex.exe");
    files.borrow_mut().push(standalone.clone());
    assert_eq!(find_codex(None, &env, &is_file).unwrap(), standalone);

    // The native exe under %APPDATA%\npm beats both.
    let default_exe = npm_native_exe(&npm_default, WINDOWS_PACKAGES[0].0, WINDOWS_PACKAGES[0].1);
    files.borrow_mut().push(default_exe.clone());
    assert_eq!(find_codex(None, &env, &is_file).unwrap(), default_exe);

    // The other architecture's package is found too.
    files.borrow_mut().retain(|p| *p != default_exe);
    let other = npm_native_exe(&npm_default, WINDOWS_PACKAGES[1].0, WINDOWS_PACKAGES[1].1);
    files.borrow_mut().push(other.clone());
    assert_eq!(find_codex(None, &env, &is_file).unwrap(), other);

    // The npm prefix of a codex.cmd in PATH beats %APPDATA%\npm.
    let custom_exe = npm_native_exe(&custom, WINDOWS_PACKAGES[0].0, WINDOWS_PACKAGES[0].1);
    files.borrow_mut().push(custom_exe.clone());
    assert_eq!(find_codex(None, &env, &is_file).unwrap(), custom_exe);

    // ...but only when that directory has the shim.
    files
        .borrow_mut()
        .retain(|p| *p != custom.join("codex.cmd"));
    assert_eq!(find_codex(None, &env, &is_file).unwrap(), other);

    // harness_path wins over everything and must exist.
    let explicit = tmp.path().join("x").join("codex.exe");
    let e = find_codex(Some(&explicit), &env, &is_file).unwrap_err();
    assert!(e.contains("harness_path"), "{e}");
    files.borrow_mut().push(explicit.clone());
    assert_eq!(
        find_codex(Some(&explicit), &env, &is_file).unwrap(),
        explicit
    );
}

#[cfg(windows)]
#[test]
fn find_codex_windows_default_layout() {
    // The layout of a real `npm install -g @openai/codex` (0.163.0, x64).
    let appdata = PathBuf::from(r"C:\Users\u\AppData\Roaming");
    let exe = appdata.join(
        r"npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe",
    );
    let env = env_of(vec![("APPDATA", appdata.into_os_string())]);
    let found = find_codex(None, &env, &|p: &Path| p == exe).unwrap();
    assert_eq!(found, exe);
}

#[cfg(unix)]
#[test]
fn find_codex_unix_path() {
    let path = std::env::join_paths(["relative", "/opt/a/bin", "/usr/local/bin"]).unwrap();
    let env = env_of(vec![("PATH", path)]);
    let files: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
    let is_file = |p: &Path| files.borrow().iter().any(|f| f == p);
    assert!(find_codex(None, &env, &is_file).is_err());
    // Relative PATH entries are ignored.
    files.borrow_mut().push(PathBuf::from("relative/codex"));
    assert!(find_codex(None, &env, &is_file).is_err());
    files
        .borrow_mut()
        .push(PathBuf::from("/usr/local/bin/codex"));
    assert_eq!(
        find_codex(None, &env, &is_file).unwrap(),
        PathBuf::from("/usr/local/bin/codex")
    );
    files.borrow_mut().push(PathBuf::from("/opt/a/bin/codex"));
    assert_eq!(
        find_codex(None, &env, &is_file).unwrap(),
        PathBuf::from("/opt/a/bin/codex")
    );
}

#[test]
fn no_path_no_codex() {
    let env = env_of(vec![]);
    assert!(find_codex(None, &env, &|_| false).is_err());
}

/// A fake codex that prints its version and the `CODEX_HOME` it got, and fails when an
/// inherited variable leaks through.
fn fake_codex(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let p = dir.join("codex.cmd");
        std::fs::write(
            &p,
            "@echo off\r\n\
             if defined AS_CODEX_LEAK exit /b 3\r\n\
             if not exist \"%CODEX_HOME%\" exit /b 4\r\n\
             echo codex-cli 0.0.0-fake home=%CODEX_HOME% arg=%1\r\n",
        )
        .unwrap();
        p
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("codex");
        std::fs::write(
            &p,
            "#!/bin/sh\n\
             [ -n \"$AS_CODEX_LEAK\" ] && exit 3\n\
             [ -d \"$CODEX_HOME\" ] || exit 4\n\
             echo \"codex-cli 0.0.0-fake home=$CODEX_HOME arg=$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

#[test]
fn version_runs_in_a_scratch_codex_home_that_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let program = fake_codex(tmp.path());
    let scratch = tmp.path().join("scratch-home");
    let inherited: Vec<(OsString, OsString)> = std::env::vars_os()
        .chain([
            (OsString::from("AS_CODEX_LEAK"), OsString::from("1")),
            (
                OsString::from("CODEX_HOME"),
                tmp.path().join("real-codex-home").into_os_string(),
            ),
        ])
        .collect();
    // Linux: a freshly written script can briefly fail with ETXTBSY; retry that.
    let mut v = Err(String::new());
    for _ in 0..20 {
        v = version_in(&program, &scratch, inherited.clone().into_iter());
        if !v.as_ref().is_err_and(|e| e.contains("os error 26")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let v = v.unwrap();
    assert!(v.starts_with("codex-cli 0.0.0-fake home="), "{v}");
    assert!(v.contains("scratch-home"), "{v}");
    assert!(v.ends_with("arg=--version"), "{v}");
    assert!(!scratch.exists(), "scratch CODEX_HOME must be removed");

    let v = CodexHarness.version(&program).unwrap();
    assert!(v.contains("agent-sidekick-codex-version-"), "{v}");
    let home = v
        .split("home=")
        .nth(1)
        .and_then(|r| r.split(" arg=").next())
        .unwrap();
    assert!(!Path::new(home).exists(), "{home} must be removed");
}

#[test]
fn version_reports_a_failing_program() {
    let tmp = tempfile::tempdir().unwrap();
    let e = version_in(
        &tmp.path().join("missing-codex"),
        &tmp.path().join("s"),
        std::env::vars_os(),
    )
    .unwrap_err();
    assert!(e.contains("starting"), "{e}");
    assert!(!tmp.path().join("s").exists());
}

#[test]
fn install_hint_names_npm_and_the_installers() {
    let hint = CodexHarness.install_hint().unwrap();
    assert!(hint.contains("npm install -g @openai/codex"));
    assert!(hint.contains("https://chatgpt.com/codex/install.sh"));
    assert!(hint.contains("https://chatgpt.com/codex/install.ps1"));
    assert_eq!(CodexHarness.name(), "codex");
}

// ----- config rules -----

#[test]
fn chatgpt_backend_resolves_with_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    assert_eq!(b.backend_type().as_str(), "codex");
    assert_eq!(b.harness_kind(), HarnessKind::Codex);
    assert_eq!(b.cost.as_str(), "quota");
    assert_eq!(b.model, None);
    assert_eq!(b.models, None);
    assert_eq!(b.secret_id(), None, "a ChatGPT login is no stored secret");
    assert!(!b.needs_provider_key());
    let BackendKind::Codex {
        auth,
        base_url,
        read_only,
        windows_sandbox,
        base_dir,
        harness_path,
        ..
    } = &b.kind
    else {
        panic!("{b:?}")
    };
    assert_eq!(*auth, CodexAuth::Chatgpt);
    assert_eq!(*base_url, None);
    assert!(!read_only);
    assert_eq!(*windows_sandbox, WindowsSandbox::Mxc);
    assert_eq!(*harness_path, None);
    assert_eq!(*base_dir, tmp.path().join("cfg").join("codex").join("gpt"));
    // Any model is accepted without an allowlist; none means Codex's default.
    assert_eq!(b.effective_model(None).unwrap(), None);
    assert_eq!(
        b.effective_model(Some("gpt-6.1-sol")).unwrap().as_deref(),
        Some("gpt-6.1-sol")
    );
}

#[test]
fn api_key_backend_with_provider_and_options() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(
        tmp.path(),
        "auth = \"api-key\"\nbase_url = \"https://gw.example/v1/\"\nwire_api = \"responses\"\n\
         models = [\"m1\", \"m2\"]\nread_only = true\nwindows_sandbox = \"unelevated\"\n\
         harness_path = \"~/bin/codex\"\n",
    )
    .unwrap();
    assert_eq!(b.cost.as_str(), "paid");
    assert_eq!(b.model.as_deref(), Some("m1"));
    assert_eq!(
        b.secret_id(),
        Some(SecretId::Backend("gpt".into())),
        "keyring user backend-gpt"
    );
    assert!(b.needs_provider_key());
    assert!(b.effective_model(Some("other")).is_err());
    let BackendKind::Codex {
        base_url,
        read_only,
        windows_sandbox,
        harness_path,
        ..
    } = &b.kind
    else {
        panic!()
    };
    assert_eq!(base_url.as_deref(), Some("https://gw.example/v1"));
    assert!(read_only);
    assert_eq!(*windows_sandbox, WindowsSandbox::Unelevated);
    assert_eq!(
        harness_path.as_deref(),
        Some(tmp.path().join("home").join("bin").join("codex").as_path())
    );

    // api_key_env instead of the keyring.
    let b = resolve_codex(tmp.path(), "auth = \"api-key\"\napi_key_env = \"MY_OAI\"\n").unwrap();
    assert_eq!(b.secret_id(), None);
    assert!(b.foreign_secret_env.contains(&"MY_OAI".to_owned()));
}

#[test]
fn codex_config_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let err = |body: &str| resolve_codex(tmp.path(), body).unwrap_err();

    // Without `auth`: the ChatGPT login.
    let b = resolve_codex(tmp.path(), "").unwrap();
    assert!(matches!(
        b.kind,
        BackendKind::Codex {
            auth: CodexAuth::Chatgpt,
            ..
        }
    ));
    let e = err("base_url = \"https://x.example/v1\"\n");
    assert!(
        e.contains("is not allowed with `auth = \"chatgpt\"`"),
        "{e}"
    );
    let e = err("auth = \"bearer\"\n");
    assert!(
        e.contains("`auth = \"bearer\"` is not valid for harness `codex`"),
        "{e}"
    );
    let e = err("auth = \"sideways\"\n");
    assert!(e.starts_with("parse error"), "{e}");
    for field in [
        "base_url = \"https://x.example/v1\"",
        "api_key_env = \"K\"",
        "wire_api = \"responses\"",
    ] {
        let e = err(&format!("auth = \"chatgpt\"\n{field}\n"));
        assert!(
            e.contains("is not allowed with `auth = \"chatgpt\"`"),
            "{field}: {e}"
        );
    }
    let e = err("auth = \"api-key\"\nbase_url = \"https://x.example/v1\"\nwire_api = \"chat\"\n");
    assert!(e.contains("Codex only speaks the Responses API"), "{e}");
    let e = err("auth = \"api-key\"\nwire_api = \"responses\"\n");
    assert!(
        e.contains("`wire_api` has no effect without `base_url`"),
        "{e}"
    );
    let e = err("auth = \"api-key\"\nbase_url = \"ftp://x\"\n");
    assert!(e.contains("must start with http:// or https://"), "{e}");
    let e = err("auth = \"chatgpt\"\nharness_path = \"codex.exe\"\n");
    assert!(e.contains("`harness_path` must be an absolute path"), "{e}");
    let e = err("auth = \"chatgpt\"\nprovider = \"ollama\"\n");
    assert!(
        e.contains("field `provider` is not allowed for harness `codex`"),
        "{e}"
    );
    let e = err("auth = \"chatgpt\"\nrequest_timeout_s = 5\n");
    assert!(
        e.contains("field `request_timeout_s` is not allowed"),
        "{e}"
    );
    let e = err("auth = \"chatgpt\"\n[backends.gpt.env]\nCODEX_HOME = \"/x\"\n");
    assert!(
        e.contains("env: `CODEX_HOME` is set by agent-sidekick"),
        "{e}"
    );
    let e = err("auth = \"chatgpt\"\npass_env = [\"OPENAI_API_KEY\"]\n");
    assert!(
        e.contains("`OPENAI_API_KEY` must not be passed through"),
        "{e}"
    );
    let e = err("auth = \"chatgpt\"\npass_env = [\"CODEX_HOME\"]\n");
    assert!(e.contains("`CODEX_HOME` must not be passed through"), "{e}");
    let e = err("auth = \"api-key\"\napi_key_env = \"CLAUDE_CODE_OAUTH_TOKEN\"\n");
    assert!(e.contains("an OAuth token must never be sent"), "{e}");
    // codex-only fields on other harnesses.
    let cfg: crate::config::Config = toml::from_str(
        "[backends.x]\nharness = \"command\"\ndescription = \"d\"\ndefault = true\n\
         command = [\"x\", \"{prompt}\"]\nwindows_sandbox = \"mxc\"\nwire_api = \"responses\"\n",
    )
    .unwrap();
    let e = crate::backends::resolve_with(&cfg, &defaults(tmp.path())).unwrap_err();
    assert!(e.contains("field `windows_sandbox` is not allowed"), "{e}");
    assert!(e.contains("field `wire_api` is not allowed"), "{e}");
    // claude-endpoint keeps its own auth values.
    let cfg: crate::config::Config = toml::from_str(
        "[backends.x]\nharness = \"claude\"\ndescription = \"d\"\ndefault = true\n\
         base_url = \"http://localhost:1\"\nauth = \"chatgpt\"\n",
    )
    .unwrap();
    let e = crate::backends::resolve_with(&cfg, &defaults(tmp.path())).unwrap_err();
    assert!(e.contains("use \"x-api-key\" or \"bearer\""), "{e}");
}

// ----- generated config -----

#[test]
fn rendered_config_isolates_and_holds_no_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let chat = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let text = render_config(&chat).unwrap();
    let v: toml::Value = toml::from_str(&text).expect("valid TOML");
    assert_eq!(v["cli_auth_credentials_store"].as_str(), Some("file"));
    assert_eq!(v["check_for_update_on_startup"].as_bool(), Some(false));
    assert_eq!(v["history"]["persistence"].as_str(), Some("none"));
    assert_eq!(v["analytics"]["enabled"].as_bool(), Some(false));
    let policy = &v["shell_environment_policy"];
    assert_eq!(policy["ignore_default_excludes"].as_bool(), Some(false));
    for p in SHELL_EXCLUDES {
        assert_eq!(policy["filters"][*p].as_str(), Some("exclude"), "{p}");
    }
    for forbidden in [
        "projects",
        "mcp_servers",
        "model_provider",
        "model_providers",
    ] {
        assert!(v.get(forbidden).is_none(), "{forbidden} in\n{text}");
    }

    let api = resolve_codex(
        tmp.path(),
        "auth = \"api-key\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let text = render_config(&api).unwrap();
    let v: toml::Value = toml::from_str(&text).expect("valid TOML");
    assert_eq!(v["model_provider"].as_str(), Some(PROVIDER_ID));
    let p = &v["model_providers"][PROVIDER_ID];
    assert_eq!(p["base_url"].as_str(), Some("http://127.0.0.1:9/v1"));
    assert_eq!(p["env_key"].as_str(), Some("AGENT_SIDEKICK_PROVIDER_KEY"));
    assert_eq!(p["wire_api"].as_str(), Some("responses"));
    assert!(!text.contains(KEY));

    // OpenAI's own API: no custom provider.
    let plain = resolve_codex(tmp.path(), "auth = \"api-key\"\n").unwrap();
    let v: toml::Value = toml::from_str(&render_config(&plain).unwrap()).unwrap();
    assert!(v.get("model_provider").is_none());
}

#[test]
fn prepare_home_writes_the_config_only_when_it_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let file = prepare_home(&b).unwrap();
    assert_eq!(
        file,
        tmp.path()
            .join("cfg")
            .join("codex")
            .join("gpt")
            .join("config.toml")
    );
    let first = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    prepare_home(&b).unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), first);
    // Only the config file: no temp files left behind.
    let names: Vec<String> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["config.toml"]);
}

// ----- launch -----

#[test]
fn chatgpt_launch_argv_env_and_header() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let program = tmp.path().join("codex.exe");
    let l = launch(&b, &Credential::None, &program, &req(&work), None).unwrap();

    let mut want = vec![
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
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    want.push(work.to_string_lossy().into_owned());
    if cfg!(windows) {
        want.push("-c".into());
        want.push("windows.sandbox=mxc".into());
    }
    want.push("-".into());
    assert_eq!(strs(&l.args), want);
    assert_eq!(l.stdin.as_deref(), Some("Fix \"it\" & test"));
    assert_eq!(l.program, program);

    let home = tmp.path().join("cfg").join("codex").join("gpt");
    assert_eq!(get(&l.env, "CODEX_HOME"), home.to_str());
    assert_eq!(get(&l.env, "CODEX_SQLITE_HOME"), home.to_str());
    assert!(home.join("config.toml").is_file());
    for gone in [
        "CLAUDECODE",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_SESSION_ID",
        "ANTHROPIC_API_KEY",
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_TOOL_CALL_ID",
        "CODEX_CI",
        "CODEX_VERSION",
        "CODEX_SANDBOX",
        "CODEX_SANDBOX_NETWORK_DISABLED",
        "CODEX_MANAGED_BY_NPM",
        "CODEX_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "RUST_LOG",
        "AGENT_SIDEKICK_PROVIDER_KEY",
    ] {
        assert_eq!(get(&l.env, gone), None, "{gone} reached codex");
    }
    for kept in ["PATH", "HOME", "CODEX_CA_CERTIFICATE", "SSL_CERT_FILE"] {
        assert!(get(&l.env, kept).is_some(), "{kept} missing");
    }
    assert!(l.secrets.is_empty());
    let header = l.header.join("\n");
    for needle in [
        "auth: chatgpt",
        "codex home: ",
        "prompt delivery: stdin",
        "sandbox: workspace-write",
        "max_turns: 7 ignored",
        "model: codex default",
        "allowed_tools ignored by the codex harness",
    ] {
        assert!(header.contains(needle), "{needle}: {header}");
    }
}

#[test]
fn api_key_launch_puts_the_key_into_the_right_variable_only() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().to_path_buf();
    let program = tmp.path().join("codex.exe");
    let cred = Credential::ProviderKey(Token::new(KEY));

    // OpenAI's API: CODEX_API_KEY for the built-in provider.
    let b = resolve_codex(tmp.path(), "auth = \"api-key\"\nread_only = true\n").unwrap();
    let l = launch(&b, &cred, &program, &req(&work), Some("gpt-6.1-sol")).unwrap();
    assert_eq!(get(&l.env, "CODEX_API_KEY"), Some(KEY));
    assert_eq!(get(&l.env, "AGENT_SIDEKICK_PROVIDER_KEY"), None);
    let args = strs(&l.args);
    let i = args.iter().position(|a| a == "--sandbox").unwrap();
    assert_eq!(args[i + 1], "read-only");
    let m = args.iter().position(|a| a == "-m").unwrap();
    assert_eq!(args[m + 1], "gpt-6.1-sol");
    assert_eq!(l.secrets.redact(&format!("x {KEY} y")), "x [REDACTED] y");
    assert!(l.header.join("\n").contains("provider openai"));

    // A custom provider: AGENT_SIDEKICK_PROVIDER_KEY (its env_key), never CODEX_API_KEY.
    let b = resolve_codex(
        tmp.path(),
        "auth = \"api-key\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let l = launch(&b, &cred, &program, &req(&work), None).unwrap();
    assert_eq!(get(&l.env, "AGENT_SIDEKICK_PROVIDER_KEY"), Some(KEY));
    assert_eq!(get(&l.env, "CODEX_API_KEY"), None);
    // The key is in no argument and in no file of the backend's CODEX_HOME.
    assert!(!strs(&l.args).iter().any(|a| a.contains(KEY)));
    let home = tmp.path().join("cfg").join("codex").join("gpt");
    for e in std::fs::read_dir(&home).unwrap() {
        let text = std::fs::read_to_string(e.unwrap().path()).unwrap_or_default();
        assert!(!text.contains(KEY));
    }
}

#[test]
fn batch_wrapper_refuses_unsafe_model_names() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let program = tmp.path().join("codex.cmd");
    if crate::locate::is_batch_script(&program) {
        let e = launch(
            &b,
            &Credential::None,
            &program,
            &req(tmp.path()),
            Some("a&b"),
        )
        .unwrap_err();
        assert!(e.contains("cmd.exe would re-parse"), "{e}");
    }
}

#[test]
fn launch_refuses_another_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg: crate::config::Config = toml::from_str(
        "[backends.x]\nharness = \"command\"\ncommand = [\"x\", \"{prompt}\"]\n\
         description = \"d\"\ndefault = true\n",
    )
    .unwrap();
    let b = crate::backends::resolve_with(&cfg, &defaults(tmp.path()))
        .unwrap()
        .get("x")
        .unwrap()
        .clone();
    let e = launch(
        &b,
        &Credential::None,
        Path::new("codex"),
        &req(tmp.path()),
        None,
    )
    .unwrap_err();
    assert!(e.contains("internal error"), "{e}");
    assert!(render_config(&b).is_err());
    assert_eq!(auth_file(&b), None);
}

// ----- credentials and reachability -----

#[test]
fn chatgpt_credential_needs_the_login_file() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let loader = |_: &SecretId| -> Result<(Token, TokenSource), TokenError> {
        panic!("no secret store access in ChatGPT mode")
    };
    let e = CodexHarness
        .load_credential(&b, &|_| None, &loader)
        .unwrap_err();
    assert!(e.contains("No ChatGPT login for backend 'gpt'"), "{e}");
    assert!(e.contains("agent-sidekick setup --backend gpt"), "{e}");

    let home = tmp.path().join("cfg").join("codex").join("gpt");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("auth.json"), "{}").unwrap();
    let (cred, source) = CodexHarness
        .load_credential(&b, &|_| None, &loader)
        .unwrap();
    assert_eq!(cred, Credential::None);
    assert!(source.contains("auth.json"), "{source}");
    assert!(CodexHarness.pre_run_check(&b, &cred).is_ok());
}

#[test]
fn api_key_credential_from_env_or_store() {
    let tmp = tempfile::tempdir().unwrap();
    let b = resolve_codex(tmp.path(), "auth = \"api-key\"\napi_key_env = \"MY_OAI\"\n").unwrap();
    let no_store = |_: &SecretId| -> Result<(Token, TokenSource), TokenError> { unreachable!() };
    let e = CodexHarness
        .load_credential(&b, &|_| None, &no_store)
        .unwrap_err();
    assert!(e.contains("MY_OAI"), "{e}");
    let (cred, src) = CodexHarness
        .load_credential(&b, &|k| (k == "MY_OAI").then(|| KEY.to_owned()), &no_store)
        .unwrap();
    assert_eq!(cred, Credential::ProviderKey(Token::new(KEY)));
    assert_eq!(src, "environment variable MY_OAI");

    let b = resolve_codex(tmp.path(), "auth = \"api-key\"\n").unwrap();
    let missing =
        |_: &SecretId| -> Result<(Token, TokenSource), TokenError> { Err(TokenError::Missing) };
    let e = CodexHarness
        .load_credential(&b, &|_| None, &missing)
        .unwrap_err();
    assert!(e.contains("setup --backend gpt"), "{e}");
    let stored = |id: &SecretId| -> Result<(Token, TokenSource), TokenError> {
        assert_eq!(*id, SecretId::Backend("gpt".into()));
        Ok((Token::new(KEY), TokenSource::Keyring))
    };
    let (cred, _) = CodexHarness
        .load_credential(&b, &|_| None, &stored)
        .unwrap();
    assert_eq!(cred, Credential::ProviderKey(Token::new(KEY)));
}

fn deps_with<'a>(
    locate: &'a dyn Fn(&Backend) -> Result<PathBuf, String>,
    secret: &'a dyn Fn(&SecretId) -> Result<String, TokenError>,
    probe_get: &'a dyn Fn(&str, Option<&str>) -> ProbeResponse,
) -> CheckDeps<'a> {
    CheckDeps {
        find_claude: &|| panic!("claude is not needed"),
        secret,
        env_lookup: &|_| None,
        resolve_program: &|_| panic!("no command program"),
        probe: &|_| panic!("no plain probe"),
        locate,
        secret_value: &|_| Ok(Token::new(KEY)),
        probe_get,
    }
}

#[test]
fn check_reports_install_login_key_and_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let found = |_: &Backend| Ok(PathBuf::from("/x/codex"));
    let stored = |_: &SecretId| Ok("OS keyring".to_owned());
    let no_probe = |_: &str, _: Option<&str>| -> ProbeResponse { panic!("no probe") };

    // Not installed.
    let b = resolve_codex(tmp.path(), "auth = \"chatgpt\"\n").unwrap();
    let missing = |_: &Backend| Err("codex not found".to_owned());
    let r = CodexHarness.check(&b, &deps_with(&missing, &stored, &no_probe));
    assert!(!r.reachable);
    assert!(r.detail.starts_with("codex not installed"), "{}", r.detail);

    // ChatGPT: login file decides, no network.
    let r = CodexHarness.check(&b, &deps_with(&found, &stored, &no_probe));
    assert!(!r.reachable);
    assert!(r.detail.contains("not logged in"), "{}", r.detail);
    let home = tmp.path().join("cfg").join("codex").join("gpt");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("auth.json"), "{}").unwrap();
    let r = CodexHarness.check(&b, &deps_with(&found, &stored, &no_probe));
    assert!(r.reachable, "{}", r.detail);
    assert!(r.detail.contains("ChatGPT login present"), "{}", r.detail);

    // api-key: the key goes as bearer to <base>/models; 401 = rejected.
    let b = resolve_codex(
        tmp.path(),
        "auth = \"api-key\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let seen: RefCell<Vec<(String, Option<String>)>> = RefCell::new(Vec::new());
    let probe = |u: &str, k: Option<&str>| {
        seen.borrow_mut().push((u.to_owned(), k.map(str::to_owned)));
        ProbeResponse::Status {
            code: 200,
            url: u.to_owned(),
            millis: 1,
        }
    };
    let r = CodexHarness.check(&b, &deps_with(&found, &stored, &probe));
    assert!(r.reachable, "{}", r.detail);
    assert!(!r.detail.contains(KEY));
    assert_eq!(
        *seen.borrow(),
        [(
            "http://127.0.0.1:9/v1/models".to_owned(),
            Some(KEY.to_owned())
        )]
    );
    let rejected = |u: &str, _: Option<&str>| ProbeResponse::Status {
        code: 401,
        url: u.to_owned(),
        millis: 1,
    };
    let r = CodexHarness.check(&b, &deps_with(&found, &stored, &rejected));
    assert!(!r.reachable);
    assert!(r.detail.contains("rejected the key"), "{}", r.detail);
    let none = |_: &SecretId| Err(TokenError::Missing);
    let r = CodexHarness.check(&b, &deps_with(&found, &none, &no_probe));
    assert!(!r.reachable);
    assert!(r.detail.contains("no API key stored"), "{}", r.detail);

    // Without base_url: OpenAI's model list.
    let b = resolve_codex(tmp.path(), "auth = \"api-key\"\n").unwrap();
    assert_eq!(
        probe_url(&b).as_deref(),
        Some("https://api.openai.com/v1/models")
    );
}

#[test]
fn login_commands_name_the_backend_home() {
    let home = Path::new("/cfg/codex/gpt");
    let text = login_commands(home);
    assert!(text.contains("codex login"), "{text}");
    assert!(text.contains("--device-auth"), "{text}");
    assert!(text.contains(&home.display().to_string()), "{text}");
    if cfg!(windows) {
        assert!(text.contains("$env:CODEX_HOME = "), "{text}");
        assert!(text.contains("Remove-Item Env:CODEX_HOME"), "{text}");
    } else {
        assert!(
            text.contains("CODEX_HOME=\"/cfg/codex/gpt\" codex login"),
            "{text}"
        );
    }
}

// ----- output -----

fn ev(v: serde_json::Value) -> String {
    v.to_string()
}

fn success_lines(answer: &str) -> String {
    use serde_json::json;
    [
        ev(json!({"type":"thread.started","thread_id":"t1"})),
        ev(json!({"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `fake-model` not found. Defaulting to fallback metadata."}})),
        ev(json!({"type":"turn.started"})),
        ev(json!({"type":"error","message":"Reconnecting... 2/5"})),
        ev(json!({"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"cargo test","aggregated_output":"","exit_code":null,"status":"in_progress"}})),
        ev(json!({"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"cargo test","aggregated_output":"ok","exit_code":0,"status":"completed"}})),
        ev(json!({"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"fake","tool":"slow","arguments":{},"result":null,"error":{"message":"timed out awaiting tools/call after 3s"},"status":"failed"}})),
        ev(json!({"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"first draft"}})),
        ev(json!({"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":answer}})),
        ev(json!({"type":"turn.completed","usage":{"input_tokens":246,"cached_input_tokens":14,"cache_write_input_tokens":0,"output_tokens":90,"reasoning_output_tokens":6}})),
    ]
    .join("\n")
        + "\n"
}

#[test]
fn normalize_success_keeps_the_answer_last_before_usage() {
    let raw = format!(
        "WARNING: proceeding, even though we could not create PATH aliases: Refusing ...\n\
         2026-10-21T10:00:00Z ERROR codex_core::tools::router: blocked by policy\n{}",
        success_lines("FINAL line1\nline2")
    );
    let n = normalize_json_lines(&raw);
    assert_eq!(n.failure, None);
    assert_eq!(
        n.text,
        "[codex activity] 1 command(s), 1 MCP tool call(s)\n\
         [command] cargo test -> exit 0 (completed)\n\
         [tool] fake/slow (failed): timed out awaiting tools/call after 3s\n\
         [warning] Model metadata for `fake-model` not found. Defaulting to fallback metadata.\n\
         [warning] Reconnecting... 2/5\n\
         [codex output]\n\
         2026-10-21T10:00:00Z ERROR codex_core::tools::router: blocked by policy\n\
         FINAL line1\nline2\n\
         [usage] tokens in 246 (cached 14) / out 90 (reasoning 6)"
    );
}

#[test]
fn normalize_turn_failed_and_missing_answers() {
    use serde_json::json;
    let raw = [
        ev(json!({"type":"thread.started","thread_id":"t1"})),
        ev(json!({"type":"turn.started"})),
        ev(json!({"type":"error","message":"Missing environment variable: `AS_MISSING_KEY`."})),
        ev(json!({"type":"turn.failed","error":{"message":"Missing environment variable: `AS_MISSING_KEY`."}})),
    ]
    .join("\n");
    let n = normalize_json_lines(&raw);
    assert_eq!(
        n.failure.as_deref(),
        Some("turn failed: Missing environment variable: `AS_MISSING_KEY`.")
    );
    assert!(n
        .text
        .ends_with("[error] turn failed: Missing environment variable: `AS_MISSING_KEY`."));

    let n = normalize_json_lines(&ev(json!({"type":"turn.completed","usage":{}})));
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: codex produced no final message")
    );
    let n = normalize_json_lines("error: unexpected argument '-e' found\n");
    assert_eq!(
        n.failure.as_deref(),
        Some("no answer: no codex JSON output")
    );
    assert_eq!(
        n.text,
        "[codex output]\nerror: unexpected argument '-e' found"
    );
}

#[test]
fn normalize_tolerates_a_cut_off_first_line_and_bounds_activity() {
    use serde_json::json;
    let mut raw = String::from("utput\":\"x\",\"exit_code\":0,\"status\":\"completed\"}}\n");
    for i in 0..30 {
        raw.push_str(&ev(json!({"type":"item.completed","item":{"type":"command_execution","command":format!("cmd {i}"),"exit_code":i,"status":"completed"}})));
        raw.push('\n');
    }
    raw.push_str(&ev(
        json!({"type":"item.completed","item":{"type":"agent_message","text":"done"}}),
    ));
    let n = normalize_json_lines(&raw);
    assert_eq!(n.failure, None);
    assert!(
        n.text
            .starts_with("[codex activity] 30 command(s), 0 MCP tool call(s) (last 12 shown)\n"),
        "{}",
        n.text
    );
    assert!(!n.text.contains("cmd 17 "), "{}", n.text);
    assert!(n.text.contains("[command] cmd 18 -> exit 18"), "{}", n.text);
    assert!(!n.text.contains("utput"), "{}", n.text);
    assert!(n.text.ends_with("done"), "{}", n.text);
}
