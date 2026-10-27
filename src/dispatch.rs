use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::backends::{self, Backend, BackendKind, OpencodeProvider, PermissionMode};
use crate::data::{DataDir, ModelRow, ProtectedKind, ProtectedRow, SessionLog};
use crate::harness::claude::{model_matches, parse_report};
use crate::harness::HarnessKind;
use crate::runner::{self, Launch, RunRequest};
use crate::trust::{Dimension, Event, ModelKey, Outcome, Scope, Sperre, Standing};

pub const GLOBAL_LIMITS: Limits = Limits { max_files: 8, max_lines: 300 };
pub const REPO_LIMITS: Limits = Limits { max_files: 15, max_lines: 600 };
pub const MODULE_LIMITS: Limits = Limits { max_files: 25, max_lines: 1000 };
pub const TEST_GLOBS: [&str; 2] = ["tests/**", "spec/**"];
pub const WRITE_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];
pub const READ_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];
pub const MAX_TURNS_SUBTYPE: &str = "error_max_turns";
pub const PATCH_BEGIN: &str = "<<<SIDEKICK-PATCH";
pub const PATCH_END: &str = "SIDEKICK-PATCH>>>";
pub const PROPOSAL_INSTRUCTION: &str = "You have no write tools in this job. Do not try to modify files. Return the complete change as one unified diff (git format, paths relative to the repository root, a/ and b/ prefixes) in your final answer, between a line containing only <<<SIDEKICK-PATCH and a line containing only SIDEKICK-PATCH>>>. Nothing else may be inside the markers.";
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
    pub checkpoint: Option<String>,
    pub quant: Option<String>,
}

pub const CLAUDE_ALIASES: [(&str, &str); 4] = [
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("fable", "claude-fable-5-1"),
];

pub fn resolve_claude_alias(model: &str) -> String {
    CLAUDE_ALIASES.iter().find(|(alias, _)| *alias == model).map_or(model, |(_, id)| id).to_string()
}

pub fn expected_claude_model(backend: &Backend, model: Option<&str>) -> Option<String> {
    matches!(backend.kind, BackendKind::ClaudeOauth { .. })
        .then(|| model.or(backend.model.as_deref()).map(resolve_claude_alias))
        .flatten()
}

impl Identity {
    pub fn key(&self, backend: &Backend, model: Option<&str>) -> ModelKey {
        let provider = default_provider(backend);
        let resolved = expected_claude_model(backend, model);
        let model = resolved.as_deref().or(model).unwrap_or("default");
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

fn url_host(base_url: &str) -> String {
    base_url
        .split("://")
        .last()
        .and_then(|r| r.split(['/', ':']).next())
        .filter(|h| !h.is_empty())
        .unwrap_or("endpoint")
        .into()
}

fn default_provider(backend: &Backend) -> String {
    match &backend.kind {
        BackendKind::ClaudeOauth { .. } => "anthropic".into(),
        BackendKind::ClaudeEndpoint { base_url, .. } => url_host(base_url),
        BackendKind::Codex { base_url: Some(url), .. } => url_host(url),
        BackendKind::Codex { .. } => "openai".into(),
        BackendKind::OpenCode { provider: OpencodeProvider::OpenaiCompatible, base_url: Some(url), .. } => url_host(url),
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
        HarnessKind::OpenCode => Vec::new(),
        _ => vec!["max_steps", "tool_output_lines", "context_budget_tokens"],
    }
}

pub fn with_registry_limits(backend: &Backend, row: &ModelRow) -> Backend {
    let mut b = backend.clone();
    if let BackendKind::OpenCode { limits, .. } = &mut b.kind {
        let positive = |v: Option<i64>| v.filter(|n| *n > 0).map(|n| n.min(u32::MAX as i64) as u32);
        limits.tool_output_lines = positive(row.tool_output_lines);
        limits.context_budget_tokens = positive(row.context_budget_tokens);
        limits.context_tokens = limits.context_tokens.or(positive(row.context_tokens));
    }
    b
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

pub fn extract_patch(text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let end_after = |i: usize| lines[i + 1..].iter().position(|l| l.trim() == PATCH_END).map(|j| i + 1 + j);
    let (begin, end) = (0..lines.len())
        .rev()
        .filter(|&i| lines[i].trim() == PATCH_BEGIN)
        .find_map(|i| end_after(i).map(|j| (i, j)))
        .ok_or("no complete patch between the SIDEKICK-PATCH markers")?;
    let mut body = &lines[begin + 1..end];
    while body.first().is_some_and(|l| l.trim_start().starts_with("```")) {
        body = &body[1..];
    }
    while body.last().is_some_and(|l| l.trim_start().starts_with("```")) {
        body = &body[..body.len() - 1];
    }
    let patch = body.join("\n");
    if !patch.lines().any(|l| l.starts_with("--- ")) || !patch.lines().any(|l| l.starts_with("+++ ")) {
        return Err("the text between the markers is not a unified diff".into());
    }
    Ok(format!("{patch}\n"))
}

pub fn glob_match_path(pattern: &str, path: &str) -> bool {
    let mut end = 0;
    for seg in path.split('/') {
        end += seg.len();
        if glob_match(pattern, &path[..end]) {
            return true;
        }
        end += 1;
    }
    false
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
                if !TEST_GLOBS.iter().any(|g| glob_match_path(g, f)) {
                    out.violations.push(format!("write-tests touches non-test path {f}"));
                }
            } else if let Some(g) = self.globs().find(|g| glob_match_path(g, f)) {
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
    NoPatch,
    Blocked,
    Aborted,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Completed => "completed",
            Status::NoChange => "nochange",
            Status::NoPatch => "nopatch",
            Status::Blocked => "blocked",
            Status::Aborted => "aborted",
            Status::Failed => "failed",
        }
    }
}

pub fn status_summary(status: Status) -> String {
    format!("status: {}", status.as_str())
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
    pub reason: Option<String>,
    pub proposal_mode: bool,
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
        git(&self.path, &["-c", "core.quotepath=false", "diff", "--cached", "--binary", "--no-color", "--no-renames", unified, &self.base])
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
        let limited = with_registry_limits(backend, &row);
        let prepared = if write_right { limited } else { withhold_writes(&limited) };
        let expected = expected_claude_model(backend, model.as_deref());
        let model = expected.clone().or(model);
        let max_turns = match (backend.harness_kind(), row.max_steps) {
            (HarnessKind::Claude | HarnessKind::OpenCode, Some(m)) if m > 0 => req.max_turns.min(m as u32),
            _ => req.max_turns,
        };
        let proposal = req.dimension.is_write() && !write_right;
        let wt = Worktree::create(&req.repo, &self.work_dir, &req.job_id)?;
        let run_req = RunRequest {
            prompt: if proposal { format!("{}\n\n{PROPOSAL_INSTRUCTION}", req.prompt) } else { req.prompt.clone() },
            cwd: wt.path.clone(),
            max_turns,
            model,
            allowed_tools: (!write_right).then(|| READ_TOOLS.map(String::from).to_vec()),
            timeout: req.timeout,
        };
        let json_output = matches!(backend.kind, BackendKind::ClaudeOauth { .. });
        let launched = (self.launcher)(&prepared, &run_req).map(|mut l| {
            if json_output {
                let at = if l.args.len() >= 2 && l.args[l.args.len() - 2] == "--" { l.args.len() - 2 } else { l.args.len() };
                l.args.splice(at..at, ["--output-format".into(), "json".into()]);
            }
            l
        }).map_err(|e| anyhow!(e));
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
            reason: None,
            proposal_mode: proposal,
            evidence_hash: None,
            event_id: None,
            exit_code: outcome.exit_code,
            duration_s: outcome.duration.as_secs_f64(),
            log_path: outcome.log_path.clone(),
            tail: outcome.tail.clone(),
        };
        let raw = runner::read_logged_output(&outcome.log_path, outcome.output_offset).unwrap_or_else(|| outcome.output.clone());
        let report = json_output.then(|| parse_report(&raw)).flatten();
        if let Some(r) = &report {
            result.tail = runner::truncate_tail(&r.text, runner::TAIL_CHARS);
        }
        let model_verified = match &expected {
            None => true,
            Some(exp) => match report.as_ref().and_then(|r| r.models.first()) {
                None => {
                    result.warnings.push(format!("model unverified: the claude CLI output exposes no model id (expected {exp})"));
                    false
                }
                Some(m) if model_matches(m, exp) => true,
                Some(m) => {
                    result.warnings.push(format!("model mismatch: expected {exp}, the claude CLI reported {m}"));
                    false
                }
            },
        };
        if !model_verified && standing.apply_without_review() {
            result.warnings.push("gold not applied without a verified model: result returned as branch/patch".into());
        }
        let mut event = Event::new(&model_id, req.dimension, req.scope.clone(), Outcome::Note, now_ms());
        event.machine_id = self.machine_id.clone();
        event.user_id = self.user_id.clone();
        event.job_id = Some(req.job_id.clone());
        event.duration_s = Some(result.duration_s);
        let finished = (|| -> Result<()> {
            let max_turns_hit = report.as_ref().is_some_and(|r| r.subtype == MAX_TURNS_SUBTYPE);
            if let Some(r) = report.as_ref().filter(|r| r.is_error && r.subtype != MAX_TURNS_SUBTYPE) {
                result.status = Status::Failed;
                let reason = format!("claude reported an error ({}): {}", r.subtype, r.text);
                result.tail = runner::truncate_tail(&reason, runner::TAIL_CHARS);
                result.reason = Some(reason);
                wt.remove(true);
                return Ok(());
            }
            let aborted = outcome.timed_out || outcome.cancelled || max_turns_hit;
            let mut files = wt.files()?;
            let mut proposed = false;
            if proposal && files.is_empty() {
                let text = match &report {
                    Some(r) => r.text.clone(),
                    None => backend.harness().normalize_output(&raw).map_or(raw.clone(), |n| n.text),
                };
                let applied = extract_patch(&text).and_then(|patch| {
                    let proposal_path = self.work_dir.join("patches").join(format!("{}.proposal", req.job_id));
                    std::fs::create_dir_all(proposal_path.parent().unwrap()).map_err(|e| e.to_string())?;
                    std::fs::write(&proposal_path, patch).map_err(|e| e.to_string())?;
                    let arg = proposal_path.to_string_lossy().into_owned();
                    let done = git(&wt.path, &["apply", "--check", "--whitespace=nowarn", &arg])
                        .and_then(|_| git(&wt.path, &["apply", "--whitespace=nowarn", &arg]))
                        .map_err(|e| format!("proposed patch does not apply: {e:#}"));
                    let _ = std::fs::remove_file(&proposal_path);
                    done
                });
                match applied {
                    Ok(_) => {
                        files = wt.files()?;
                        proposed = true;
                    }
                    Err(_) if aborted => {}
                    Err(reason) => {
                        result.status = Status::NoPatch;
                        result.reason = Some(reason);
                        wt.remove(true);
                        return Ok(());
                    }
                }
            }
            if files.is_empty() {
                if aborted {
                    result.status = Status::Aborted;
                    event.outcome = Outcome::Aborted;
                } else {
                    result.status = Status::NoChange;
                }
                wt.remove(true);
                return Ok(());
            }
            let patch = wt.diff("-U3")?;
            if !write_right && !proposed {
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
            let gold = standing.apply_without_review() && matches!(backend.kind, BackendKind::ClaudeOauth { .. }) && model_verified;
            if !check.violations.is_empty() && (!gold || aborted) {
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
            if aborted {
                result.status = Status::Aborted;
                event.outcome = Outcome::Aborted;
                wt.remove(true);
                return Ok(());
            }
            result.warnings.extend(check.violations);
            let patches = self.work_dir.join("patches");
            std::fs::create_dir_all(&patches)?;
            let patch_path = patches.join(format!("{}.patch", req.job_id));
            std::fs::write(&patch_path, &patch)?;
            if gold {
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
        if result.status == Status::Failed {
            return Ok((result, None));
        }
        event.ts_ms = now_ms();
        event.summary = Some(status_summary(result.status));
        result.event_id = Some(event.event_id.clone());
        Ok((result, Some(event)))
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
        assert!(glob_match_path("**/validate*", "src/validate/mod.rs"));
        assert!(!glob_match_path("**/validate*", "src/revalidate/mod.rs"));
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
