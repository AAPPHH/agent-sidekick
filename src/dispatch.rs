use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::backends::{self, Backend, BackendKind, PermissionMode};
use crate::data::{DataDir, ProtectedKind, ProtectedRow, SessionLog};
use crate::harness::HarnessKind;
use crate::runner::{self, Launch, RunRequest};
use crate::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre, Standing};

pub const GLOBAL_LIMITS: Limits = Limits { max_files: 8, max_lines: 300 };
pub const REPO_LIMITS: Limits = Limits { max_files: 15, max_lines: 600 };
pub const MODULE_LIMITS: Limits = Limits { max_files: 25, max_lines: 1000 };
pub const TEST_GLOBS: [&str; 2] = ["tests/**", "spec/**"];
pub const WRITE_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];
pub const READ_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];
pub const MAX_TURNS_MARKER: &str = "reached max turns";
pub const BRANCH_PREFIX: &str = "sidekick/";
pub(crate) const GIT_IDENT: [&str; 4] = ["-c", "user.name=agent-sidekick", "-c", "user.email=agent-sidekick@localhost"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_lines: usize,
}

impl Limits {
    pub fn for_scope(scope: &Scope) -> Limits {
        match scope {
            Scope::Global => GLOBAL_LIMITS,
            Scope::Repo(_) => REPO_LIMITS,
            Scope::Module(..) => MODULE_LIMITS,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub provider: Option<String>,
    pub checkpoint: Option<String>,
    pub quant: Option<String>,
}

impl Identity {
    pub fn key(&self, backend: &Backend, model: Option<&str>) -> ModelKey {
        let provider = self.provider.clone().unwrap_or_else(|| default_provider(backend));
        let model = model.unwrap_or("default");
        let checkpoint = self.checkpoint.as_deref().unwrap_or(model);
        let quant = self.quant.as_deref().unwrap_or("none");
        ModelKey::new(&provider, model, checkpoint, quant, harness_name(backend))
    }
}

pub fn harness_name(backend: &Backend) -> &'static str {
    match backend.harness_kind() {
        HarnessKind::Claude => "claude-code",
        other => other.as_str(),
    }
}

fn default_provider(backend: &Backend) -> String {
    match &backend.kind {
        BackendKind::ClaudeOauth { .. } => "anthropic".into(),
        BackendKind::ClaudeEndpoint { base_url, .. } => base_url
            .split("://")
            .last()
            .and_then(|r| r.split(['/', ':']).next())
            .unwrap_or("endpoint")
            .into(),
        BackendKind::Codex { .. } => "openai".into(),
        BackendKind::OpenCode { provider, .. } => provider.as_str().into(),
        BackendKind::Command { command, .. } => Path::new(&command[0])
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "command".into()),
    }
}

pub fn technical_sperre(backend: &Backend) -> Sperre {
    match backend.harness_kind() {
        HarnessKind::Command => Sperre::NurDiff,
        _ => Sperre::Technisch,
    }
}

pub fn unsupported_limits(backend: &Backend) -> Vec<&'static str> {
    match backend.harness_kind() {
        HarnessKind::Claude => vec!["tool_output_lines", "context_budget_tokens"],
        _ => vec!["max_steps", "tool_output_lines", "context_budget_tokens"],
    }
}

pub fn withhold_writes(backend: &Backend) -> Backend {
    let mut b = backend.clone();
    match &mut b.kind {
        BackendKind::ClaudeOauth { permissions, .. } | BackendKind::ClaudeEndpoint { permissions, .. } => {
            if matches!(permissions.mode, PermissionMode::AcceptEdits | PermissionMode::Auto | PermissionMode::BypassPermissions) {
                permissions.mode = PermissionMode::Default;
            }
            permissions.allowed_tools = Some(READ_TOOLS.map(String::from).to_vec());
            for t in WRITE_TOOLS {
                if !permissions.disallowed_tools.iter().any(|d| d == t) {
                    permissions.disallowed_tools.push(t.into());
                }
            }
        }
        BackendKind::OpenCode { read_only, .. } | BackendKind::Codex { read_only, .. } => *read_only = true,
        BackendKind::Command { .. } => {}
    }
    b
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                if p.get(2) == Some(&b'/') && go(&p[3..], s) {
                    return true;
                }
                (0..=s.len()).any(|i| go(&p[2..], &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && s[0] != b'/' && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), path.as_bytes())
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffCheck {
    pub files: Vec<String>,
    pub lines: usize,
    pub violations: Vec<String>,
}

pub struct DiffChecker<'a> {
    pub protected: &'a [ProtectedRow],
    pub dimension: Dimension,
    pub limits: Limits,
    pub scope: &'a Scope,
}

impl DiffChecker<'_> {
    fn globs(&self) -> impl Iterator<Item = &str> {
        self.protected.iter().filter(|p| p.kind == ProtectedKind::PathGlob).map(|p| p.value.as_str())
    }

    pub fn check(&self, files: &[String], unified0: &str) -> DiffCheck {
        let mut out = DiffCheck { files: files.to_vec(), ..Default::default() };
        let module = match self.scope {
            Scope::Module(_, path) => Some(path.trim_matches('/')),
            _ => None,
        };
        for f in files {
            if let Some(m) = module.filter(|m| f != m && !f.starts_with(&format!("{m}/"))) {
                out.violations.push(format!("path {f} outside module path {m}"));
            }
            if self.dimension == Dimension::WriteTests {
                if !TEST_GLOBS.iter().any(|g| glob_match(g, f)) {
                    out.violations.push(format!("write-tests touches non-test path {f}"));
                }
            } else if let Some(g) = self.globs().find(|g| glob_match(g, f)) {
                out.violations.push(format!("protected path {f} (matches {g})"));
            }
        }
        let terms: Vec<&str> =
            self.protected.iter().filter(|p| p.kind == ProtectedKind::RemovedTerm).map(|p| p.value.as_str()).collect();
        let mut file = String::new();
        let mut header = false;
        let mut counts: Vec<(String, usize, usize)> = Vec::new();
        for line in unified0.lines() {
            if line.starts_with("diff --git ") {
                header = true;
                file.clear();
                continue;
            }
            if header {
                if line.starts_with("@@") {
                    header = false;
                } else if let Some(n) = line.strip_prefix("--- a/").or_else(|| line.strip_prefix("+++ b/")) {
                    file = n.to_string();
                }
                continue;
            }
            let (sign, body) = match line.split_at_checked(1) {
                Some(("+", b)) => (true, b),
                Some(("-", b)) => (false, b),
                _ => continue,
            };
            out.lines += 1;
            for (i, t) in terms.iter().enumerate() {
                let n = body.matches(t).count();
                if n == 0 {
                    continue;
                }
                let key = format!("{file}\0{i}");
                let slot = match counts.iter().position(|c| c.0 == key) {
                    Some(p) => p,
                    None => {
                        counts.push((key, 0, 0));
                        counts.len() - 1
                    }
                };
                if sign {
                    counts[slot].2 += n;
                } else {
                    counts[slot].1 += n;
                }
            }
        }
        for (key, removed, added) in counts {
            if removed > added {
                let (f, i) = key.split_once('\0').unwrap();
                let term = terms[i.parse::<usize>().unwrap()];
                out.violations.push(format!("removed protected term `{term}` in {f}"));
            }
        }
        if files.len() > self.limits.max_files {
            out.violations.push(format!("{} files exceed limit {}", files.len(), self.limits.max_files));
        }
        if out.lines > self.limits.max_lines {
            out.violations.push(format!("{} changed lines exceed limit {}", out.lines, self.limits.max_lines));
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Completed,
    NoChange,
    Blocked,
    Aborted,
}

#[derive(Clone, Debug)]
pub struct DispatchRequest {
    pub prompt: String,
    pub repo: PathBuf,
    pub dimension: Dimension,
    pub scope: Scope,
    pub job_id: String,
    pub model: Option<String>,
    pub max_turns: u32,
    pub timeout: Duration,
    pub identity: Identity,
}

#[derive(Clone, Debug)]
pub struct DispatchResult {
    pub status: Status,
    pub model_id: String,
    pub write_right: bool,
    pub write_tools_withheld: bool,
    pub sperre: Sperre,
    pub sperre_note: Option<String>,
    pub unsupported_limits: Vec<&'static str>,
    pub branch: Option<String>,
    pub patch_path: Option<PathBuf>,
    pub applied: bool,
    pub diff_discarded: bool,
    pub violations: Vec<String>,
    pub warnings: Vec<String>,
    pub block_reason: Option<String>,
    pub evidence_hash: Option<String>,
    pub event_id: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_s: f64,
    pub log_path: PathBuf,
    pub tail: String,
}

pub type Launcher = Box<dyn Fn(&Backend, &RunRequest) -> Result<Launch, String> + Send + Sync>;

pub struct Dispatcher {
    pub data: DataDir,
    pub local_root: PathBuf,
    pub work_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub machine_id: String,
    pub user_id: String,
    pub launcher: Launcher,
}

pub fn real_launcher(claude_path: Option<PathBuf>, pass_env: Vec<String>) -> Launcher {
    Box::new(move |backend, req| {
        let (cred, _) = backends::load_credential(backend)?;
        let program = backends::resolve_program(backend, claude_path.as_deref())?;
        backend.harness().pre_run_check(backend, &cred)?;
        backends::build_launch(backend, req, &cred, &program, std::env::vars_os(), &pass_env)
    })
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").env("GIT_TERMINAL_PROMPT", "0").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
    base: String,
}

impl Worktree {
    fn create(repo: &Path, work_dir: &Path, job_id: &str) -> Result<Self> {
        if job_id.is_empty() || job_id.contains(['/', '\\', ' ']) || job_id.starts_with('.') {
            bail!("invalid job_id `{job_id}`");
        }
        let base = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
        let path = work_dir.join("worktrees").join(job_id);
        let branch = format!("{BRANCH_PREFIX}{job_id}");
        std::fs::create_dir_all(path.parent().unwrap())?;
        git(repo, &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "HEAD"])?;
        Ok(Self { repo: repo.to_path_buf(), path, branch, base })
    }

    fn diff(&self, unified: &str) -> Result<String> {
        git(&self.path, &["add", "-A"])?;
        git(&self.path, &["diff", "--cached", "--binary", "--no-color", "--no-renames", unified, &self.base])
    }

    fn files(&self) -> Result<Vec<String>> {
        git(&self.path, &["add", "-A"])?;
        let out = git(&self.path, &["diff", "--cached", "--name-only", "-z", "--no-renames", &self.base])?;
        Ok(out.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
    }

    fn remove(&self, drop_branch: bool) {
        let _ = git(&self.repo, &["worktree", "remove", "--force", &self.path.to_string_lossy()]);
        if drop_branch {
            let _ = git(&self.repo, &["branch", "-D", &self.branch]);
        }
    }

    fn commit(&self, message: &str) -> Result<()> {
        let mut args: Vec<&str> = GIT_IDENT.to_vec();
        args.extend(["commit", "-q", "-m", message]);
        git(&self.path, &args).map(|_| ())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Dispatcher {
    pub async fn dispatch(&self, backend: &Backend, req: &DispatchRequest, log: &mut SessionLog) -> Result<DispatchResult> {
        let (result, event) = self.run(backend, req, CancellationToken::new()).await?;
        if let Some(event) = event {
            log.append(event)?;
        }
        Ok(result)
    }

    pub async fn run(&self, backend: &Backend, req: &DispatchRequest, cancel: CancellationToken) -> Result<(DispatchResult, Option<Event>)> {
        let model = backend.effective_model(req.model.as_deref()).map_err(|e| anyhow!(e))?;
        let key = req.identity.key(backend, model.as_deref());
        let model_id = key.model_id();
        let row = self.data.model(&model_id).map_err(|_| {
            anyhow!(
                "backend `{}` is not registered: no models.parquet row for {}/{}/{}/{}/{} (model_id {model_id}); add it to the registry first",
                backend.name, key.provider, key.model, key.checkpoint, key.quant, key.harness
            )
        })?;
        let technical = technical_sperre(backend);
        let sperre = if row.sperre == Sperre::Technisch && technical == Sperre::Technisch { Sperre::Technisch } else { Sperre::NurDiff };
        let sperre_note = (sperre != row.sperre).then(|| {
            format!("registry says {} but the {} harness cannot withhold write tools; treated as nur-diff", row.sperre.as_str(), key.harness)
        });
        let events = self.data.events(Some(&self.local_root))?;
        let standing = Standing::compute(&key, sperre, &events, now_ms());
        let write_right = req.dimension.is_write() && standing.write_right(req.dimension, &req.scope);
        let protected = self.data.protected()?;
        let prepared = if write_right { backend.clone() } else { withhold_writes(backend) };
        let max_turns = match (backend.harness_kind(), row.max_steps) {
            (HarnessKind::Claude, Some(m)) if m > 0 => req.max_turns.min(m as u32),
            _ => req.max_turns,
        };
        let wt = Worktree::create(&req.repo, &self.work_dir, &req.job_id)?;
        let run_req = RunRequest {
            prompt: req.prompt.clone(),
            cwd: wt.path.clone(),
            max_turns,
            model,
            allowed_tools: (!write_right).then(|| READ_TOOLS.map(String::from).to_vec()),
            timeout: req.timeout,
        };
        let launched = (self.launcher)(&prepared, &run_req).map_err(|e| anyhow!(e));
        let outcome = match launched {
            Ok(launch) => runner::run_launch(launch, run_req, &self.logs_dir, cancel).await.map_err(anyhow::Error::from),
            Err(e) => Err(e),
        };
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                wt.remove(true);
                return Err(e);
            }
        };
        let mut result = DispatchResult {
            status: Status::Completed,
            model_id: model_id.clone(),
            write_right,
            write_tools_withheld: !write_right,
            sperre,
            sperre_note,
            unsupported_limits: unsupported_limits(backend),
            branch: None,
            patch_path: None,
            applied: false,
            diff_discarded: false,
            violations: Vec::new(),
            warnings: Vec::new(),
            block_reason: None,
            evidence_hash: None,
            event_id: None,
            exit_code: outcome.exit_code,
            duration_s: outcome.duration.as_secs_f64(),
            log_path: outcome.log_path.clone(),
            tail: outcome.tail.clone(),
        };
        let mut event = Event::new(&model_id, req.dimension, req.scope.clone(), Outcome::Note, now_ms());
        event.machine_id = self.machine_id.clone();
        event.user_id = self.user_id.clone();
        event.job_id = Some(req.job_id.clone());
        event.duration_s = Some(result.duration_s);
        let finished = (|| -> Result<()> {
            let aborted = outcome.timed_out || outcome.cancelled || outcome.output.to_lowercase().contains(MAX_TURNS_MARKER);
            let files = wt.files()?;
            if aborted {
                result.status = Status::Aborted;
                event.outcome = Outcome::Aborted;
                wt.remove(true);
                return Ok(());
            }
            if files.is_empty() {
                result.status = Status::NoChange;
                wt.remove(true);
                return Ok(());
            }
            let patch = wt.diff("-U3")?;
            if !write_right {
                let reason = format!("diff without write right ({}): {}", req.dimension, files.join(", "));
                result.status = Status::Blocked;
                result.diff_discarded = true;
                result.evidence_hash = Some(sha256_hex(patch.as_bytes()));
                event.outcome = Outcome::Blocked;
                event.block_reason = Some(reason.clone());
                event.evidence_hash = result.evidence_hash.clone();
                result.violations = vec![reason.clone()];
                result.block_reason = Some(reason);
                wt.remove(true);
                return Ok(());
            }
            let checker = DiffChecker { protected: &protected, dimension: req.dimension, limits: Limits::for_scope(&req.scope), scope: &req.scope };
            let check = checker.check(&files, &wt.diff("-U0")?);
            if !check.violations.is_empty() && !standing.apply_without_review() {
                let reason = check.violations.join("; ");
                result.status = Status::Blocked;
                result.evidence_hash = Some(sha256_hex(patch.as_bytes()));
                event.outcome = Outcome::Blocked;
                event.block_reason = Some(reason.clone());
                event.evidence_hash = result.evidence_hash.clone();
                result.block_reason = Some(reason);
                result.violations = check.violations;
                wt.remove(true);
                return Ok(());
            }
            result.warnings = check.violations;
            let patches = self.work_dir.join("patches");
            std::fs::create_dir_all(&patches)?;
            let patch_path = patches.join(format!("{}.patch", req.job_id));
            std::fs::write(&patch_path, &patch)?;
            if standing.apply_without_review() {
                match git(&req.repo, &["apply", "--whitespace=nowarn", &patch_path.to_string_lossy()]) {
                    Ok(_) => {
                        result.applied = true;
                        wt.remove(true);
                    }
                    Err(e) => result.warnings.push(format!("gold apply failed, branch and patch kept: {e:#}")),
                }
            }
            if !result.applied {
                wt.commit(&format!("sidekick job {}", req.job_id))?;
                wt.remove(false);
                result.branch = Some(wt.branch.clone());
            }
            result.patch_path = Some(patch_path);
            Ok(())
        })();
        if let Err(e) = finished {
            wt.remove(result.branch.is_none());
            return Err(e);
        }
        let event = matches!(result.status, Status::Blocked | Status::Aborted).then(|| {
            result.event_id = Some(event.event_id.clone());
            event
        });
        Ok((result, event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("tests/**", "tests/a/b.rs"));
        assert!(!glob_match("tests/**", "src/tests.rs"));
        assert!(glob_match("**/validate*", "validate.rs"));
        assert!(glob_match("**/validate*", "src/x/validate_input.rs"));
        assert!(!glob_match("**/validate*", "src/revalidate.rs"));
        assert!(glob_match("src/*.rs", "src/a.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
    }

    #[test]
    fn term_removal_needs_readd() {
        let protected = ProtectedRow::defaults();
        let checker = DiffChecker { protected: &protected, dimension: Dimension::WriteCode, limits: GLOBAL_LIMITS, scope: &Scope::Global };
        let removed = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-assert!(x);\n+x;\n";
        assert_eq!(checker.check(&["src/a.rs".into()], removed).violations.len(), 1);
        let moved = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-assert!(x);\n+assert!(y);\n";
        assert!(checker.check(&["src/a.rs".into()], moved).violations.is_empty());
    }
}
