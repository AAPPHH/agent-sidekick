//! Finding the `claude` executable. MCP hosts (e.g. the VS Code extension) often start
//! servers with a reduced PATH, so well-known install locations are searched too.
//!
//! Assumptions:
//! - `npm prefix -g` is never spawned (slow, and `npm` may not be on PATH); only the usual
//!   npm global locations and `NPM_CONFIG_PREFIX`/`npm_config_prefix` are checked.
//! - An empty `AGENT_SIDEKICK_CLAUDE` is treated as unset.
//! - On Windows only `claude.exe`, `claude.cmd` and `claude.bat` are accepted from PATH and
//!   well-known dirs (in that order); the extensionless `claude` sh shim npm installs next to
//!   `claude.cmd` cannot be started by `CreateProcess`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub const ENV_CLAUDE: &str = "AGENT_SIDEKICK_CLAUDE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateSource {
    Env,
    Config,
    Path,
    WellKnown,
}

impl fmt::Display for LocateSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LocateSource::Env => "AGENT_SIDEKICK_CLAUDE",
            LocateSource::Config => "config.toml claude_path",
            LocateSource::Path => "PATH",
            LocateSource::WellKnown => "well-known install location",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Absolute path to the executable (`claude`, `claude.exe` or `claude.cmd`).
    pub path: PathBuf,
    pub source: LocateSource,
}

#[derive(Debug, thiserror::Error)]
pub enum LocateError {
    #[error("{var} points to {path}, which does not exist or is not a file")]
    BadOverride { var: String, path: PathBuf },
    #[error("could not find the `claude` executable (searched: {searched}); install Claude Code or set AGENT_SIDEKICK_CLAUDE to its absolute path")]
    NotFound { searched: String },
}

/// Platform flavour that decides file names and search locations (injectable for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Unix,
}

impl Os {
    pub const fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else {
            Os::Unix
        }
    }

    /// Candidate file names in preference order. `.exe` first: it needs no `cmd.exe`.
    fn exe_names(self) -> &'static [&'static str] {
        match self {
            Os::Windows => &["claude.exe", "claude.cmd", "claude.bat"],
            Os::Unix => &["claude"],
        }
    }
}

/// Order: `AGENT_SIDEKICK_CLAUDE`, `config_path` (config.toml), PATH, well-known locations.
pub fn find_claude(config_path: Option<&Path>) -> Result<Located, LocateError> {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    find_claude_with(
        &|k| std::env::var_os(k),
        home.as_deref(),
        config_path,
        Os::current(),
    )
}

/// Testable core of [`find_claude`]: all environment access goes through `env`.
pub fn find_claude_with(
    env: &dyn Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
    config_path: Option<&Path>,
    os: Os,
) -> Result<Located, LocateError> {
    if let Some(p) = env(ENV_CLAUDE).filter(|v| !v.is_empty()) {
        return check_override(Path::new(&p), ENV_CLAUDE, LocateSource::Env);
    }
    if let Some(p) = config_path {
        return check_override(p, "config.toml claude_path", LocateSource::Config);
    }

    let path_dirs: Vec<PathBuf> = env("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|d| !d.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(path) = path_dirs.iter().find_map(|d| find_in_dir(d, os)) {
        return Ok(Located {
            path,
            source: LocateSource::Path,
        });
    }

    let known = well_known_dirs(env, home, os);
    if let Some(path) = known.iter().find_map(|d| find_in_dir(d, os)) {
        return Ok(Located {
            path,
            source: LocateSource::WellKnown,
        });
    }

    let mut searched = vec![format!("PATH ({} dirs)", path_dirs.len())];
    searched.extend(known.iter().map(|d| d.display().to_string()));
    Err(LocateError::NotFound {
        searched: searched.join(", "),
    })
}

fn check_override(p: &Path, var: &str, source: LocateSource) -> Result<Located, LocateError> {
    let path = absolute(p);
    if path.is_file() {
        Ok(Located { path, source })
    } else {
        Err(LocateError::BadOverride {
            var: var.to_string(),
            path,
        })
    }
}

/// Absolute path without canonicalizing (avoids `\\?\` prefixes on Windows).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn find_in_dir(dir: &Path, os: Os) -> Option<PathBuf> {
    os.exe_names()
        .iter()
        .map(|name| dir.join(name))
        .find(|c| is_executable_file(c, os))
        .map(|c| absolute(&c))
}

fn is_executable_file(p: &Path, os: Os) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    if os == Os::Unix {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    let _ = os;
    true
}

/// Common install locations, in search order, deduplicated.
fn well_known_dirs(
    env: &dyn Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
    os: Os,
) -> Vec<PathBuf> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(h) = home {
        dirs.push(h.join(".local").join("bin"));
        dirs.push(h.join(".claude").join("local"));
        dirs.push(h.join(".npm-global").join("bin"));
    }
    match os {
        Os::Unix => {
            for k in ["NPM_CONFIG_PREFIX", "npm_config_prefix"] {
                if let Some(prefix) = var(k) {
                    dirs.push(prefix.join("bin"));
                }
            }
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            if let Some(h) = home {
                dirs.push(h.join(".volta").join("bin"));
                dirs.push(h.join(".bun").join("bin"));
            }
        }
        Os::Windows => {
            // On Windows the npm global prefix itself holds the shims (no `bin` subdir).
            for k in ["NPM_CONFIG_PREFIX", "npm_config_prefix"] {
                if let Some(prefix) = var(k) {
                    dirs.push(prefix);
                }
            }
            if let Some(appdata) = var("APPDATA") {
                dirs.push(appdata.join("npm"));
            }
            if let Some(local) = var("LOCALAPPDATA") {
                let programs = local.join("Programs");
                dirs.push(programs.join("claude"));
                dirs.push(programs.join("claude-code"));
                dirs.push(programs);
            }
            if let Some(h) = home {
                dirs.push(h.join(".volta").join("bin"));
                dirs.push(h.join(".bun").join("bin"));
            }
        }
    }
    let mut seen = Vec::with_capacity(dirs.len());
    dirs.retain(|d| {
        let new = !seen.contains(d);
        if new {
            seen.push(d.clone());
        }
        new
    });
    dirs
}

/// True if the file must be started through `cmd.exe` (Windows `.cmd`/`.bat` shims).
pub fn is_batch_script(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Creates an "executable" file (mode 0755 on unix).
    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn native_name() -> &'static str {
        if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        }
    }

    fn run(
        vars: &[(&str, OsString)],
        home: Option<&Path>,
        config: Option<&Path>,
    ) -> Result<Located, LocateError> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        find_claude_with(&|k| map.get(k).cloned(), home, config, Os::current())
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn found_on_path() {
        let empty = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        let exe = touch(bin.path(), native_name());
        let r = run(
            &[("PATH", path_of(&[empty.path(), bin.path()]))],
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.source, LocateSource::Path);
        assert_eq!(r.path, exe);
        assert!(r.path.is_absolute());
    }

    #[test]
    fn env_override_wins_over_path() {
        let bin = TempDir::new().unwrap();
        touch(bin.path(), native_name());
        let other = TempDir::new().unwrap();
        let custom = touch(other.path(), "my-claude");
        let r = run(
            &[
                ("PATH", path_of(&[bin.path()])),
                (ENV_CLAUDE, custom.clone().into()),
            ],
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            r,
            Located {
                path: custom,
                source: LocateSource::Env
            }
        );
    }

    #[test]
    fn invalid_env_override_is_error() {
        let bin = TempDir::new().unwrap();
        touch(bin.path(), native_name());
        let missing = bin.path().join("nope");
        let err = run(
            &[
                ("PATH", path_of(&[bin.path()])),
                (ENV_CLAUDE, missing.clone().into()),
            ],
            None,
            None,
        )
        .unwrap_err();
        match err {
            LocateError::BadOverride { var, path } => {
                assert_eq!(var, ENV_CLAUDE);
                assert_eq!(path, missing);
            }
            e => panic!("unexpected {e:?}"),
        }
        // A directory is not a valid override either.
        let err = run(&[(ENV_CLAUDE, bin.path().into())], None, None).unwrap_err();
        assert!(matches!(err, LocateError::BadOverride { .. }));
    }

    #[test]
    fn config_override_used_and_validated() {
        let bin = TempDir::new().unwrap();
        let custom = touch(bin.path(), "claude-custom");
        let r = run(&[], None, Some(&custom)).unwrap();
        assert_eq!(r.source, LocateSource::Config);
        let err = run(&[], None, Some(&bin.path().join("missing"))).unwrap_err();
        match err {
            LocateError::BadOverride { var, .. } => assert_eq!(var, "config.toml claude_path"),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn empty_env_override_is_ignored() {
        let bin = TempDir::new().unwrap();
        touch(bin.path(), native_name());
        let r = run(
            &[("PATH", path_of(&[bin.path()])), (ENV_CLAUDE, "".into())],
            None,
            None,
        );
        assert_eq!(r.unwrap().source, LocateSource::Path);
    }

    #[cfg(windows)]
    #[test]
    fn exe_preferred_over_cmd_on_windows() {
        let bin = TempDir::new().unwrap();
        touch(bin.path(), "claude.cmd");
        let exe = touch(bin.path(), "claude.exe");
        let r = run(&[("PATH", path_of(&[bin.path()]))], None, None).unwrap();
        assert_eq!(r.path, exe);
    }

    #[cfg(windows)]
    #[test]
    fn extensionless_shim_ignored_on_windows() {
        let shim_dir = TempDir::new().unwrap();
        touch(shim_dir.path(), "claude");
        let err = run(&[("PATH", path_of(&[shim_dir.path()]))], None, None).unwrap_err();
        assert!(matches!(err, LocateError::NotFound { .. }));

        let cmd = touch(shim_dir.path(), "claude.cmd");
        let r = run(&[("PATH", path_of(&[shim_dir.path()]))], None, None).unwrap();
        assert_eq!(r.path, cmd);
        assert!(is_batch_script(&r.path));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_ignored_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let bin = TempDir::new().unwrap();
        let p = touch(bin.path(), "claude");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = run(&[("PATH", path_of(&[bin.path()]))], None, None).unwrap_err();
        assert!(matches!(err, LocateError::NotFound { .. }));
    }

    #[test]
    fn falls_back_to_well_known_dir() {
        let home = TempDir::new().unwrap();
        let local_bin = home.path().join(".local").join("bin");
        std::fs::create_dir_all(&local_bin).unwrap();
        let exe = touch(&local_bin, native_name());
        let r = run(&[], Some(home.path()), None).unwrap();
        assert_eq!(
            r,
            Located {
                path: exe,
                source: LocateSource::WellKnown
            }
        );
    }

    #[test]
    fn not_found_lists_searched_places() {
        let home = TempDir::new().unwrap();
        let empty = TempDir::new().unwrap();
        // Os::Windows with no APPDATA/LOCALAPPDATA searches only PATH and the fake home, so a
        // real install in e.g. /usr/local/bin on the test machine cannot interfere.
        let path = path_of(&[empty.path()]);
        let env = |k: &str| (k == "PATH").then(|| path.clone());
        let err = find_claude_with(&env, Some(home.path()), None, Os::Windows).unwrap_err();
        match err {
            LocateError::NotFound { searched } => {
                assert!(searched.starts_with("PATH (1 dirs)"), "{searched}");
                assert!(searched.contains(".local"), "{searched}");
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn well_known_dirs_are_deduplicated() {
        let env =
            |k: &str| (k == "npm_config_prefix" || k == "NPM_CONFIG_PREFIX").then(|| "/p".into());
        let dirs = well_known_dirs(&env, None, Os::Unix);
        assert_eq!(
            dirs.iter().filter(|d| **d == Path::new("/p/bin")).count(),
            1
        );
    }

    #[test]
    fn batch_script_detection() {
        assert!(is_batch_script(Path::new(r"C:\npm\claude.cmd")));
        assert!(is_batch_script(Path::new("claude.BAT")));
        assert!(!is_batch_script(Path::new("claude.exe")));
        assert!(!is_batch_script(Path::new("claude")));
    }
}
