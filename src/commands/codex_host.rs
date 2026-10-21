//! agent-sidekick as an MCP server of the OpenAI Codex CLI ("Codex as host").
//!
//! `agent-sidekick setup --codex` writes `[mcp_servers.agent-sidekick]` into the user's Codex
//! config (`$CODEX_HOME/config.toml`, default `~/.codex/config.toml`), `uninstall` removes
//! it and `doctor` checks it. The file is edited directly (with `toml_edit`, after a backup)
//! instead of through `codex mcp add`, because `codex mcp add` (0.163.0) can set neither
//! `tool_timeout_sec` nor `startup_timeout_sec`, drops them on a re-add, and rewrites other
//! tables (it removed an inline comment of another server). Everything outside the
//! agent-sidekick table stays byte for byte as it was; this is checked before writing.
//!
//! Why the timeout matters: Codex cancels an MCP tool call after `tool_timeout_sec`
//! (default 60 s) and, unlike Claude Code, progress notifications do not extend it
//! (verified with codex 0.163.0). A `sidekick_run` may take up to `timeout_s` (max
//! [`MAX_TIMEOUT_S`]) twice (with a fallback), so the registration uses
//! [`TOOL_TIMEOUT_SEC`].
//!
//! Codex starts MCP servers with an allowlisted environment (on Windows: APPDATA, PATH,
//! USERPROFILE, TEMP and a few system variables). What agent-sidekick needs beyond that goes
//! into the table: `AGENT_SIDEKICK_CLAUDE` (and the config dir overrides, if set) as `env`
//! values (paths, no secrets), the variables named by `api_key_env`/`pass_env` and the usual
//! proxy/certificate variables as `env_vars` (names only: Codex forwards their values from
//! its own environment, so no secret is written into the file).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use toml_edit::{value, Array, DocumentMut, Item, Table};

use crate::config::{Config, MAX_TIMEOUT_S};
use crate::locate::ENV_CLAUDE;
use crate::APP_NAME;

/// The server's name in Codex (`[mcp_servers.<name>]`).
pub const SERVER_NAME: &str = APP_NAME;
/// Tools that only read: registered with `approval_mode = "approve"`, so Codex calls them
/// without asking (it asks before every MCP tool call by default and refuses them in
/// `codex exec`). `sidekick_run` keeps Codex's default (ask first): it starts another agent
/// that works outside Codex's sandbox.
pub const READ_ONLY_TOOLS: [&str; 2] = ["sidekick_backends", "sidekick_status"];
/// `tool_timeout_sec` of the registration: two attempts of the longest allowed run (a
/// fallback gets its own full timeout) plus ten minutes for queueing and startup.
pub const TOOL_TIMEOUT_SEC: u64 = 2 * MAX_TIMEOUT_S + 600;
/// `startup_timeout_sec` (Codex's default of 10 s can be short on a cold start with a
/// keyring prompt).
pub const STARTUP_TIMEOUT_SEC: u64 = 30;

/// Variables Codex should forward to the server when they are set in its environment
/// (`env_vars`), besides those named in config.toml: proxies and CA bundles (for provider
/// probes and the agents), the locale, Claude Code's Git Bash location (a claude backend on
/// Windows may need it and Codex's allowlist drops it), and on Unix what the OS keyring
/// (Secret Service over D-Bus) and the XDG config dir need.
const FORWARDED_VARS: &[&str] = &[
    "CLAUDE_CODE_GIT_BASH_PATH",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "LANG",
];
#[cfg(unix)]
const FORWARDED_VARS_UNIX: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
];
#[cfg(not(unix))]
const FORWARDED_VARS_UNIX: &[&str] = &[];

/// agent-sidekick's own location overrides, written as `env` values when they are set in the
/// environment `setup --codex` runs in (paths, never secrets).
const PATH_OVERRIDES: &[&str] = &[
    crate::paths::ENV_CONFIG_DIR,
    crate::paths::ENV_CLAUDE_CONFIG_DIR,
];

/// The user's Codex config file: `$CODEX_HOME/config.toml` when `CODEX_HOME` is set (and not
/// empty), else `<home>/.codex/config.toml`. `None`: neither is known.
pub fn config_path(env: &dyn Fn(&str) -> Option<OsString>, home: Option<&Path>) -> Option<PathBuf> {
    match env("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join("config.toml")),
        None => home.map(|h| h.join(".codex").join("config.toml")),
    }
}

/// [`config_path`] for the real environment.
pub fn real_config_path() -> Option<PathBuf> {
    config_path(
        &|k| std::env::var_os(k),
        crate::paths::home_dir().ok().as_deref(),
    )
}

/// What the registration contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// This executable (absolute).
    pub command: PathBuf,
    /// `env` values (non-secret paths): `AGENT_SIDEKICK_CLAUDE` and set path overrides.
    pub env: Vec<(String, String)>,
    /// `env_vars`: names Codex forwards from its own environment.
    pub env_vars: Vec<String>,
}

impl Registration {
    /// The registration for `command` with the claude found by setup (if any), the path
    /// overrides set in `env_lookup`, and the variable names `cfg` needs.
    pub fn new(
        command: &Path,
        claude: Option<&Path>,
        cfg: Option<&Config>,
        env_lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> Self {
        let mut env = Vec::new();
        if let Some(c) = claude {
            env.push((ENV_CLAUDE.to_owned(), c.to_string_lossy().into_owned()));
        }
        for var in PATH_OVERRIDES {
            if let Some(v) = env_lookup(var).filter(|v| !v.is_empty()) {
                env.push(((*var).to_owned(), v.to_string_lossy().into_owned()));
            }
        }
        Self {
            command: command.to_path_buf(),
            env,
            env_vars: needed_vars(cfg),
        }
    }
}

/// The variable names the server needs from Codex's environment: every `api_key_env`, the
/// global and per-backend `pass_env`, and [`FORWARDED_VARS`] (sorted, without duplicates).
pub fn needed_vars(cfg: Option<&Config>) -> Vec<String> {
    let mut vars: Vec<String> = FORWARDED_VARS
        .iter()
        .chain(FORWARDED_VARS_UNIX)
        .map(|s| (*s).to_owned())
        .collect();
    if let Some(cfg) = cfg {
        vars.extend(config_vars(cfg));
    }
    vars.sort();
    vars.dedup();
    vars
}

/// The variable names config.toml makes the server read from its environment: every
/// `api_key_env` and the global and per-backend `pass_env` (sorted, without duplicates).
pub fn config_vars(cfg: &Config) -> Vec<String> {
    let mut vars: Vec<String> = cfg.pass_env.clone();
    for b in cfg.backends.iter().flat_map(|m| m.values()) {
        vars.extend(b.api_key_env.iter().cloned());
        vars.extend(b.pass_env.iter().flatten().cloned());
    }
    vars.retain(|v| !v.is_empty() && !v.contains('='));
    vars.sort();
    vars.dedup();
    vars
}

/// The registration as it is in a Codex config (for doctor).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub command: Option<String>,
    pub args: Vec<String>,
    /// `enabled`; absent means enabled.
    pub enabled: bool,
    /// `tool_timeout_sec`; `None`: Codex's default of 60 s.
    pub tool_timeout_sec: Option<f64>,
    /// `env` keys and values (paths only, written by setup or the user).
    pub env: Vec<(String, String)>,
    pub env_vars: Vec<String>,
}

impl Entry {
    pub fn claude(&self) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == ENV_CLAUDE)
            .map(|(_, v)| v.as_str())
    }
}

/// The agent-sidekick entry of a Codex config text. `Ok(None)`: not registered; `Err`: the
/// text is not valid TOML.
pub fn read_entry(text: &str) -> Result<Option<Entry>, String> {
    let v: toml::Value = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
    let Some(t) = v
        .get("mcp_servers")
        .and_then(|s| s.get(SERVER_NAME))
        .and_then(toml::Value::as_table)
    else {
        return Ok(None);
    };
    let strings = |key: &str| -> Vec<String> {
        t.get(key)
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        x.as_str()
                            .or_else(|| x.get("name").and_then(toml::Value::as_str))
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let tool_timeout_sec = t
        .get("tool_timeout_sec")
        .and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)));
    Ok(Some(Entry {
        command: t
            .get("command")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        args: strings("args"),
        enabled: t
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        tool_timeout_sec,
        env: t
            .get("env")
            .and_then(toml::Value::as_table)
            .map(|e| {
                e.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        env_vars: strings("env_vars"),
    }))
}

/// `text` without the agent-sidekick table (for the "nothing else changed" check).
fn without_entry(text: &str) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("invalid TOML: {e}"))?;
    if let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_like_mut) {
        servers.remove(SERVER_NAME);
    }
    Ok(doc.to_string())
}

/// A TOML basic string.
fn quoted(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// The table text appended for a new registration.
fn new_table_text(reg: &Registration) -> String {
    let mut out = format!(
        "[mcp_servers.{SERVER_NAME}]\n\
         # Written by `{APP_NAME} setup --codex`. tool_timeout_sec covers the longest \
         sidekick_run\n\
         # (Codex cancels tool calls after 60 s by default; progress does not extend that).\n\
         command = {}\nargs = [\"serve\"]\nstartup_timeout_sec = {STARTUP_TIMEOUT_SEC}\n\
         tool_timeout_sec = {TOOL_TIMEOUT_SEC}\n",
        quoted(&reg.command.to_string_lossy())
    );
    if !reg.env_vars.is_empty() {
        let names: Vec<String> = reg.env_vars.iter().map(|v| quoted(v)).collect();
        out.push_str(&format!("env_vars = [{}]\n", names.join(", ")));
    }
    for tool in READ_ONLY_TOOLS {
        out.push_str(&format!(
            "\n[mcp_servers.{SERVER_NAME}.tools.{tool}]\napproval_mode = \"approve\"\n"
        ));
    }
    if !reg.env.is_empty() {
        out.push_str(&format!("\n[mcp_servers.{SERVER_NAME}.env]\n"));
        for (k, v) in &reg.env {
            let bare = !k.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            let key = if bare { k.clone() } else { quoted(k) };
            out.push_str(&format!("{key} = {}\n", quoted(v)));
        }
    }
    out
}

/// The Codex config `text` with agent-sidekick registered per `reg`:
/// - not registered yet: the table is appended (the existing text is kept byte for byte);
/// - registered as a table: `command`, `args`, both timeouts, `env_vars` (merged with the
///   existing names) and the `env` values of `reg` are set in place; other keys (`enabled`,
///   `tools.*`, further `env` values, comments) are kept.
///
/// Checked before returning: the result parses, reads back as `reg`, and everything except the
/// agent-sidekick table is unchanged. `Err`: a layout this function does not edit (e.g.
/// `mcp_servers` as an inline table) or invalid TOML, with a next step.
pub fn register_in(text: &str, reg: &Registration) -> Result<String, String> {
    let doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("the Codex config is not valid TOML ({e}); fix it first"))?;
    let existing = doc.get("mcp_servers").and_then(|s| s.get(SERVER_NAME));
    let out = match existing {
        None => {
            if let Some(servers) = doc.get("mcp_servers") {
                if !servers.is_table() {
                    return Err(
                        "`mcp_servers` in the Codex config is not a [table] (e.g. an inline \
                         table); add the agent-sidekick entry by hand (see docs/REFERENCE.md)"
                            .to_owned(),
                    );
                }
            }
            let mut out = text.to_owned();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&new_table_text(reg));
            out
        }
        Some(item) if item.is_table() => {
            let mut doc = doc;
            let table = doc["mcp_servers"][SERVER_NAME]
                .as_table_mut()
                .expect("checked: a table");
            update_table(table, reg);
            doc.to_string()
        }
        Some(_) => {
            return Err(format!(
                "`mcp_servers.{SERVER_NAME}` in the Codex config is not a [table]; remove it \
                 and run `{APP_NAME} setup --codex` again"
            ))
        }
    };
    // Verify: valid, our values, nothing else changed.
    let entry = read_entry(&out)?.ok_or("internal error: the entry was not written")?;
    let command = reg.command.to_string_lossy();
    if entry.command.as_deref() != Some(command.as_ref())
        || entry.args != ["serve"]
        || entry.tool_timeout_sec != Some(TOOL_TIMEOUT_SEC as f64)
        || reg.env.iter().any(|kv| !entry.env.contains(kv))
    {
        return Err("internal error: the written entry does not read back".to_owned());
    }
    if without_entry(&out)? != without_entry(text)? {
        return Err(
            "internal error: registering would change other parts of the Codex config; \
             nothing was written"
                .to_owned(),
        );
    }
    Ok(out)
}

/// Set the registration's values in an existing table.
fn update_table(table: &mut Table, reg: &Registration) {
    table["command"] = value(reg.command.to_string_lossy().into_owned());
    let mut args = Array::new();
    args.push("serve");
    table["args"] = value(args);
    // `startup_timeout_ms` is an alias; two settings would be ambiguous.
    table.remove("startup_timeout_ms");
    table["startup_timeout_sec"] = value(STARTUP_TIMEOUT_SEC as i64);
    table["tool_timeout_sec"] = value(TOOL_TIMEOUT_SEC as i64);
    let mut names: Vec<String> = table
        .get("env_vars")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let has_tables = table
        .get("env_vars")
        .and_then(Item::as_array)
        .is_some_and(|a| a.iter().any(|v| !v.is_str()));
    if !has_tables {
        names.extend(reg.env_vars.iter().cloned());
        names.sort();
        names.dedup();
        if !names.is_empty() {
            let mut arr = Array::new();
            for n in &names {
                arr.push(n.as_str());
            }
            table["env_vars"] = value(arr);
        }
    }
    // Read-only tools without approval prompt, unless the user chose a mode.
    if !table.contains_key("tools") {
        let mut tools = Table::new();
        tools.set_implicit(true);
        table.insert("tools", Item::Table(tools));
    }
    if let Some(tools) = table.get_mut("tools").and_then(Item::as_table_like_mut) {
        for tool in READ_ONLY_TOOLS {
            if tools.get(tool).is_none() {
                let mut t = Table::new();
                t.insert("approval_mode", value("approve"));
                tools.insert(tool, Item::Table(t));
            } else if let Some(t) = tools.get_mut(tool).and_then(Item::as_table_like_mut) {
                if t.get("approval_mode").is_none() {
                    t.insert("approval_mode", value("approve"));
                }
            }
        }
    }
    if !reg.env.is_empty() {
        if !table.contains_key("env") {
            let mut env = Table::new();
            env.set_implicit(false);
            table.insert("env", Item::Table(env));
        }
        if let Some(env) = table.get_mut("env").and_then(Item::as_table_like_mut) {
            for (k, v) in &reg.env {
                env.insert(k, value(v.as_str()));
            }
        }
    }
}

/// The Codex config `text` without the agent-sidekick entry. `Ok(None)`: it had none. Only
/// the entry's table (and its subtables) is removed; checked: every remaining line is a line
/// of the original, in the same order.
pub fn unregister_in(text: &str) -> Result<Option<String>, String> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("the Codex config is not valid TOML ({e}); fix it first"))?;
    let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_like_mut) else {
        return Ok(None);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(None);
    }
    let out = doc.to_string();
    let mut old = text.lines();
    let only_removed = out.lines().all(|l| old.any(|o| o == l));
    if !only_removed || read_entry(&out)?.is_some() {
        return Err(
            "internal error: removing the entry would change other parts of the Codex config; \
             nothing was written"
                .to_owned(),
        );
    }
    Ok(Some(out))
}

/// `config.toml.bak-agent-sidekick-<timestamp>` next to `path` (with a counter if needed).
fn backup_path(path: &Path, timestamp: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_owned());
    let mut candidate = path.with_file_name(format!("{name}.bak-{APP_NAME}-{timestamp}"));
    let mut n = 1;
    while candidate.exists() {
        n += 1;
        candidate = path.with_file_name(format!("{name}.bak-{APP_NAME}-{timestamp}-{n}"));
    }
    candidate
}

/// What writing the Codex config did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// The file already had exactly this content; nothing was written.
    Unchanged,
    /// Written; the previous file was copied to `backup` (`None`: there was no file).
    Changed { backup: Option<PathBuf> },
}

/// Write `new_text` to `path` (through a temp file and a rename; parent directories are
/// created). An existing file is first copied to a backup. A symlinked `path` is written
/// through to its target (the link stays). On Unix the new file keeps the old file's mode
/// (the config can hold other MCP servers' secrets); a new file gets 0600.
pub fn write_with_backup(path: &Path, new_text: &str, timestamp: &str) -> std::io::Result<Written> {
    let old = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if old.as_deref() == Some(new_text) {
        return Ok(Written::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let backup = match old {
        Some(_) => {
            let b = backup_path(path, timestamp);
            std::fs::copy(path, &b)?;
            Some(b)
        }
        None => None,
    };
    // Replace the link's target, not the link. A dangling link (no old content) is
    // replaced by a regular file.
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() && backup.is_some() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let tmp = target.with_extension(format!("toml.tmp-{}", std::process::id()));
    write_tmp(&tmp, new_text, &target)
        .and_then(|()| std::fs::rename(&tmp, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    Ok(Written::Changed { backup })
}

/// Write the temp file for [`write_with_backup`]; on Unix with the mode of `original`
/// (0600 if it does not exist), set before any content is written.
fn write_tmp(tmp: &Path, text: &str, original: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mode = match std::fs::metadata(original) {
            Ok(m) => m.permissions().mode() & 0o7777,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0o600,
            Err(e) => return Err(e),
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        // `mode` above is masked by the umask; set the exact mode explicitly.
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = original;
        std::fs::write(tmp, text)
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// Register in the Codex config at `path` (created if missing).
pub fn register(path: &Path, reg: &Registration, timestamp: &str) -> Result<Written, String> {
    let text = read_optional(path)?.unwrap_or_default();
    let new_text = register_in(&text, reg)?;
    write_with_backup(path, &new_text, timestamp)
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Remove the entry from the Codex config at `path`. `Ok(None)`: nothing to remove (no
/// file or no entry).
pub fn unregister(path: &Path, timestamp: &str) -> Result<Option<Written>, String> {
    let Some(text) = read_optional(path)? else {
        return Ok(None);
    };
    let Some(new_text) = unregister_in(&text)? else {
        return Ok(None);
    };
    write_with_backup(path, &new_text, timestamp)
        .map(Some)
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The entry in the Codex config at `path`. `Ok(None)`: no file or no entry.
pub fn read_entry_at(path: &Path) -> Result<Option<Entry>, String> {
    match read_optional(path)? {
        Some(text) => read_entry(&text).map_err(|e| format!("{}: {e}", path.display())),
        None => Ok(None),
    }
}

/// Timestamp for backup names.
pub fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Problems of an existing registration (empty: fine). `self_exe`: this binary; `needed`:
/// variable names the config needs from Codex (each must be in `env_vars` or `env`).
pub fn entry_problems(entry: &Entry, self_exe: Option<&Path>, needed: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if !entry.enabled {
        problems.push("it is disabled (`enabled = false`)".to_owned());
    }
    match entry.tool_timeout_sec {
        None => problems.push(
            "tool_timeout_sec is not set, so Codex cancels every sidekick_run after 60 s"
                .to_owned(),
        ),
        Some(t) if t < TOOL_TIMEOUT_SEC as f64 => problems.push(format!(
            "tool_timeout_sec is {t}, below {TOOL_TIMEOUT_SEC}; longer runs are cancelled by \
             Codex"
        )),
        _ => {}
    }
    if let (Some(cmd), Some(exe)) = (entry.command.as_deref(), self_exe) {
        if !same_file(Path::new(cmd), exe) {
            problems.push(format!(
                "command is {cmd}, but this binary is {}",
                exe.display()
            ));
        }
    }
    if entry.args != ["serve"] {
        problems.push(format!("args are {:?}, expected [\"serve\"]", entry.args));
    }
    if let Some(c) = entry.claude() {
        if !Path::new(c).is_file() {
            problems.push(format!("{ENV_CLAUDE} points to {c}, which does not exist"));
        }
    }
    let missing: Vec<&str> = needed
        .iter()
        .filter(|n| {
            !entry.env_vars.iter().any(|v| v == *n) && !entry.env.iter().any(|(k, _)| k == *n)
        })
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        problems.push(format!(
            "Codex does not pass {} to the server (not in env_vars)",
            missing.join(", ")
        ));
    }
    problems
}

fn same_file(a: &Path, b: &Path) -> bool {
    let eq = |a: &Path, b: &Path| {
        if cfg!(windows) {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        } else {
            a == b
        }
    };
    eq(a, b)
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if eq(&x, &y)
        )
}

#[cfg(test)]
mod tests;
