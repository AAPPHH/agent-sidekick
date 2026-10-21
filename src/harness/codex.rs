//! The `codex` harness: a STUB for the OpenAI Codex CLI (`@openai/codex`).
//!
//! What works: locating the installed CLI, `codex --version` and the install hint, so the
//! harness shows up in `agent-sidekick harness list` / `harness install codex`. What does not:
//! running it. OpenAI/Codex backends are not wired up yet (no model list, no key handling),
//! so `harness = "codex"` in a backend is a config error (see
//! [`crate::backends::resolve`]) and there is no internal backend type for it; no
//! [`Backend`] ever reaches this adapter. Its backend methods refuse with
//! [`NOT_ENABLED`] anyway (defence in depth): it never loads a secret and never builds a
//! launch.
//!
//! The launch facts known for the later adapter are recorded in NOTES.md ("Harness adapters
//! (0.4.0)", step 3).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{
    CheckDeps, Harness, HarnessKind, LaunchInput, SecretLoader, VERSION_ENV_KEEP, VERSION_TIMEOUT,
};
use crate::backends::launch::{eq_name, set_var};
use crate::backends::{Backend, Credential, Reachability};
use crate::runner::Launch;

/// Official install command (npm package `@openai/codex`). Printed only, never run.
pub const INSTALL_HINT: &str = "\
npm (all platforms):   npm install -g @openai/codex
(see https://www.npmjs.com/package/@openai/codex)";

/// Why a backend cannot use the codex harness (yet).
pub const NOT_ENABLED: &str = "codex harness is not enabled yet (stub)";

/// The config error for `harness = "codex"` in a backend.
pub fn config_error() -> String {
    format!(
        "{NOT_ENABLED}: agent-sidekick only locates the Codex CLI (`agent-sidekick harness \
         list`) and cannot run it; use harness claude, opencode or command"
    )
}

/// npm platform packages of `@openai/codex` on Windows: (package, target triple). The
/// native executable is
/// `<npm prefix>/node_modules/@openai/codex/node_modules/@openai/<package>/vendor/<triple>/bin/codex.exe`.
const WINDOWS_PACKAGES: [(&str, &str); 2] = if cfg!(target_arch = "aarch64") {
    [
        ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        ("codex-win32-x64", "x86_64-pc-windows-msvc"),
    ]
} else {
    [
        ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
    ]
};

/// Adapter of the `codex` harness (stub).
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexHarness;

/// The native `codex.exe` of the npm package below the npm prefix `prefix` (Windows).
fn npm_native_exe(prefix: &Path, package: &str, triple: &str) -> PathBuf {
    prefix
        .join("node_modules")
        .join("@openai")
        .join("codex")
        .join("node_modules")
        .join("@openai")
        .join(package)
        .join("vendor")
        .join(triple)
        .join("bin")
        .join("codex.exe")
}

/// Where the Codex CLI is, in this order:
/// 1. Windows: the native executable of the npm package (see [`npm_native_exe`]) below the
///    directory of a `codex.cmd` shim found in PATH, then below `%APPDATA%\npm`; then
///    `codex.exe` in an absolute PATH entry. A `.cmd`/`.ps1` shim is never returned.
/// 2. Unix: `codex` in an absolute PATH entry (the npm bin link or a native install).
///
/// `env` reads the (inherited) environment, `is_file` the filesystem; injected for tests.
pub fn find_codex(
    env: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let path_dirs: Vec<PathBuf> = env("PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|d| !d.as_os_str().is_empty() && d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        let mut prefixes: Vec<PathBuf> = path_dirs
            .iter()
            .filter(|d| is_file(&d.join("codex.cmd")))
            .cloned()
            .collect();
        if let Some(appdata) = env("APPDATA").filter(|v| !v.is_empty()) {
            prefixes.push(PathBuf::from(appdata).join("npm"));
        }
        for prefix in &prefixes {
            for (package, triple) in WINDOWS_PACKAGES {
                candidates.push(npm_native_exe(prefix, package, triple));
            }
        }
        candidates.extend(path_dirs.iter().map(|d| d.join("codex.exe")));
    } else {
        candidates.extend(path_dirs.iter().map(|d| d.join("codex")));
    }
    candidates.into_iter().find(|c| is_file(c)).ok_or_else(|| {
        "codex not found (npm global package, PATH; `.cmd` shims are not used); \
         `agent-sidekick harness install codex` shows how to install it"
            .to_owned()
    })
}

/// [`find_codex`] on the real environment; on Unix the file must be executable.
fn locate_real() -> Result<PathBuf, String> {
    let path = find_codex(&|k| std::env::var_os(k), &|p| p.is_file())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exec = std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if !exec {
            return Err(format!("{} is not executable (chmod +x)", path.display()));
        }
    }
    Ok(path)
}

/// The refusal of every backend-related method.
fn refuse(backend: &Backend) -> String {
    format!("backend {}: {NOT_ENABLED}", backend.name)
}

/// `<program> --version` in a minimal environment (of `inherited`, only
/// [`VERSION_ENV_KEEP`]) whose `CODEX_HOME` (and `HOME`/`USERPROFILE`) is the fresh
/// directory `scratch`, which is removed afterwards: codex 0.163.0 creates `tmp/` in
/// `CODEX_HOME` even for `--version`, and the user's real `~/.codex` must not be touched.
pub(crate) fn version_in(
    program: &Path,
    scratch: &Path,
    inherited: impl Iterator<Item = (OsString, OsString)>,
) -> Result<String, String> {
    std::fs::create_dir_all(scratch).map_err(|e| format!("creating {}: {e}", scratch.display()))?;
    let mut env: Vec<(OsString, OsString)> = inherited
        .filter(|(k, _)| VERSION_ENV_KEEP.iter().any(|n| eq_name(k, n)))
        .collect();
    for var in ["CODEX_HOME", "HOME", "USERPROFILE"] {
        set_var(&mut env, var, scratch.as_os_str());
    }
    let out = crate::commands::claude_cli::run_in_env(
        program,
        ["--version"],
        Some(&env),
        VERSION_TIMEOUT,
    );
    let _ = std::fs::remove_dir_all(scratch);
    let out = out.map_err(|e| format!("{e:#}"))?;
    if !out.success() {
        return Err(format!("`--version` failed: {}", out.diagnostics()));
    }
    let text = crate::redact::redact_all(out.stdout.trim());
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    /// No backend can use the stub.
    fn locate(&self, backend: &Backend, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
        Err(refuse(backend))
    }

    fn locate_installed(&self, _claude_path: Option<&Path>) -> Option<Result<PathBuf, String>> {
        Some(locate_real())
    }

    fn version(&self, program: &Path) -> Result<String, String> {
        let scratch = std::env::temp_dir().join(format!(
            "agent-sidekick-codex-version-{}",
            uuid::Uuid::new_v4().simple()
        ));
        version_in(program, &scratch, std::env::vars_os())
    }

    /// Never loads a secret.
    fn load_credential(
        &self,
        backend: &Backend,
        _env_lookup: &dyn Fn(&str) -> Option<String>,
        _secret_loader: SecretLoader<'_>,
    ) -> Result<(Credential, String), String> {
        Err(refuse(backend))
    }

    /// Never builds a launch.
    fn build_launch(&self, input: LaunchInput<'_>) -> Result<Launch, String> {
        Err(refuse(input.backend))
    }

    fn check(&self, backend: &Backend, _deps: &CheckDeps<'_>) -> Reachability {
        Reachability::unreachable(refuse(backend))
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some(INSTALL_HINT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn env_of(vars: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    }

    #[cfg(windows)]
    #[test]
    fn find_codex_windows_order() {
        let tmp = tempfile::tempdir().unwrap();
        let appdata = tmp.path().join("AppData").join("Roaming");
        let npm_default = appdata.join("npm");
        let custom = tmp.path().join("custom-npm");
        let plain = tmp.path().join("plain");
        let path = std::env::join_paths([&plain, &custom, &PathBuf::from("relative")]).unwrap();
        let env = env_of(vec![
            ("PATH", path),
            ("APPDATA", appdata.clone().into_os_string()),
        ]);
        let files: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let is_file = |p: &Path| files.borrow().iter().any(|f| f == p);

        // Nothing installed.
        let e = find_codex(&env, &is_file).unwrap_err();
        assert!(e.contains("codex not found"), "{e}");
        assert!(e.contains("harness install codex"), "{e}");

        // A shim alone is never used, neither the .cmd nor the .ps1.
        files.borrow_mut().push(custom.join("codex.cmd"));
        files.borrow_mut().push(custom.join("codex.ps1"));
        files.borrow_mut().push(npm_default.join("codex.cmd"));
        assert!(find_codex(&env, &is_file).is_err());

        // A codex.exe in PATH is the last resort.
        files.borrow_mut().push(plain.join("codex.exe"));
        assert_eq!(find_codex(&env, &is_file).unwrap(), plain.join("codex.exe"));

        // The native exe under %APPDATA%\npm beats PATH.
        let default_exe =
            npm_native_exe(&npm_default, WINDOWS_PACKAGES[0].0, WINDOWS_PACKAGES[0].1);
        files.borrow_mut().push(default_exe.clone());
        assert_eq!(find_codex(&env, &is_file).unwrap(), default_exe);
        assert!(default_exe.ends_with(
            r"npm\node_modules\@openai\codex\node_modules\@openai\".to_owned()
                + WINDOWS_PACKAGES[0].0
                + r"\vendor\"
                + WINDOWS_PACKAGES[0].1
                + r"\bin\codex.exe"
        ));

        // The other architecture's package is found too.
        files.borrow_mut().retain(|p| *p != default_exe);
        let other = npm_native_exe(&npm_default, WINDOWS_PACKAGES[1].0, WINDOWS_PACKAGES[1].1);
        files.borrow_mut().push(other.clone());
        assert_eq!(find_codex(&env, &is_file).unwrap(), other);

        // The npm prefix of a codex.cmd in PATH beats %APPDATA%\npm.
        let custom_exe = npm_native_exe(&custom, WINDOWS_PACKAGES[0].0, WINDOWS_PACKAGES[0].1);
        files.borrow_mut().push(custom_exe.clone());
        assert_eq!(find_codex(&env, &is_file).unwrap(), custom_exe);

        // ...but only when that directory has the shim.
        files
            .borrow_mut()
            .retain(|p| *p != custom.join("codex.cmd"));
        assert_eq!(find_codex(&env, &is_file).unwrap(), other);
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
        let found = find_codex(&env, &|p: &Path| p == exe).unwrap();
        assert_eq!(found, exe);
    }

    #[cfg(unix)]
    #[test]
    fn find_codex_unix_path() {
        let path = std::env::join_paths(["relative", "/opt/a/bin", "/usr/local/bin"]).unwrap();
        let env = env_of(vec![("PATH", path)]);
        let files: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let is_file = |p: &Path| files.borrow().iter().any(|f| f == p);
        assert!(find_codex(&env, &is_file).is_err());
        // Relative PATH entries are ignored.
        files.borrow_mut().push(PathBuf::from("relative/codex"));
        assert!(find_codex(&env, &is_file).is_err());
        files
            .borrow_mut()
            .push(PathBuf::from("/usr/local/bin/codex"));
        assert_eq!(
            find_codex(&env, &is_file).unwrap(),
            PathBuf::from("/usr/local/bin/codex")
        );
        files.borrow_mut().push(PathBuf::from("/opt/a/bin/codex"));
        assert_eq!(
            find_codex(&env, &is_file).unwrap(),
            PathBuf::from("/opt/a/bin/codex")
        );
    }

    #[test]
    fn no_path_no_codex() {
        let env = env_of(vec![]);
        assert!(find_codex(&env, &|_| true).is_err());
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
        // An inherited variable outside VERSION_ENV_KEEP must not reach the child (the fake
        // exits 3 if it does), nor an inherited CODEX_HOME.
        let inherited = std::env::vars_os().chain([
            (OsString::from("AS_CODEX_LEAK"), OsString::from("1")),
            (
                OsString::from("CODEX_HOME"),
                tmp.path().join("real-codex-home").into_os_string(),
            ),
        ]);
        let v = version_in(&program, &scratch, inherited).unwrap();
        assert!(v.starts_with("codex-cli 0.0.0-fake home="), "{v}");
        assert!(v.contains("scratch-home"), "{v}");
        assert!(v.ends_with("arg=--version"), "{v}");
        assert!(!scratch.exists(), "scratch CODEX_HOME must be removed");

        // Through the trait: a fresh temp dir, also removed.
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

    fn command_backend() -> Backend {
        let cfg: crate::config::Config = toml::from_str(
            "[backends.x]\nharness = \"command\"\ncommand = [\"x\", \"{prompt}\"]\n\
             description = \"d\"\ndefault = true\n",
        )
        .unwrap();
        crate::backends::resolve(&cfg)
            .unwrap()
            .iter()
            .next()
            .unwrap()
            .clone()
    }

    #[test]
    fn stub_refuses_every_backend_method() {
        let b = command_backend();
        let want = format!("backend x: {NOT_ENABLED}");
        assert_eq!(CodexHarness.locate(&b, None).unwrap_err(), want);
        let loader_called = std::cell::Cell::new(false);
        let loader = |_: &crate::token::SecretId| {
            loader_called.set(true);
            Err(crate::token::TokenError::Missing)
        };
        let env_called = std::cell::Cell::new(false);
        let env_lookup = |_: &str| {
            env_called.set(true);
            None
        };
        assert_eq!(
            CodexHarness
                .load_credential(&b, &env_lookup, &loader)
                .unwrap_err(),
            want
        );
        assert!(
            !loader_called.get() && !env_called.get(),
            "no secret access"
        );

        let req = crate::runner::RunRequest {
            prompt: "p".into(),
            cwd: std::env::temp_dir(),
            max_turns: 3,
            model: None,
            allowed_tools: None,
            timeout: std::time::Duration::from_secs(5),
        };
        let token = crate::token::Token::new("sk-should-never-be-used-12345");
        let cred = Credential::OauthToken(token);
        let e = CodexHarness
            .build_launch(LaunchInput {
                backend: &b,
                req: &req,
                credential: &cred,
                program: Path::new("codex"),
                base: vec![],
                global_pass_env: &[],
                model: None,
                header: vec![],
            })
            .unwrap_err();
        assert_eq!(e, want);

        let fail = |_: &str| -> Result<PathBuf, String> { unreachable!() };
        let deps = CheckDeps {
            find_claude: &|| unreachable!(),
            secret: &|_| unreachable!(),
            env_lookup: &|_| unreachable!(),
            resolve_program: &fail,
            probe: &|_| unreachable!(),
            locate: &|_| unreachable!(),
            secret_value: &|_| unreachable!(),
            probe_get: &|_, _| unreachable!(),
        };
        let r = CodexHarness.check(&b, &deps);
        assert!(!r.reachable);
        assert_eq!(r.detail, want);
    }

    #[test]
    fn install_hint_is_the_npm_package() {
        assert!(CodexHarness
            .install_hint()
            .unwrap()
            .contains("npm install -g @openai/codex"));
        assert_eq!(CodexHarness.name(), "codex");
    }

    #[test]
    fn config_error_names_the_stub() {
        assert!(config_error().starts_with("codex harness is not enabled yet (stub)"));
    }
}
