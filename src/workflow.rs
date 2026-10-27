use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tokio::task::JoinSet;

use crate::ask::Session;
use crate::dispatch::{git, Status, BRANCH_PREFIX, GIT_IDENT};
use crate::redact::redact_all;
use crate::server::{DispatchParams, JobExtras, JobReport, Prepared, SidekickServer, Snapshot};
use crate::trust::Dimension;

const TAIL_PER_STEP: usize = 1500;

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct StepParams {
    /// Unique id of the step (letters, digits, `-` and `_`); part of the step's job_id.
    pub id: String,
    /// Self-contained task for this step. The results of the `depends_on` steps are put in front of it.
    pub prompt: String,
    /// search | inventory | write-tests | write-code | decisions.
    pub dimension: String,
    /// global (default) | repo:<name> | module:<repo>/<path>.
    pub scope: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
    /// Ids of the steps that must finish cleanly first.
    pub depends_on: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct WorkflowParams {
    pub steps: Vec<StepParams>,
    /// Git repository the workflow works on. Default: the server's current directory.
    pub cwd: Option<String>,
    /// Name of the workflow and its branch (`sidekick/<workflow_id>`). Default: generated.
    pub workflow_id: Option<String>,
}

struct Planned {
    id: String,
    depends_on: Vec<String>,
    prep: Prepared,
    dimension: Dimension,
}

pub struct Plan {
    pub id: String,
    repo: PathBuf,
    steps: Vec<Planned>,
}

impl Plan {
    pub fn total_timeout(&self) -> Duration {
        self.steps.iter().map(|s| s.prep.dreq.timeout).sum()
    }
}

struct StepResult {
    id: String,
    job_id: String,
    status: String,
    ok: bool,
    writing: bool,
    gold: bool,
    merged: bool,
    reason: Option<String>,
    model_id: Option<String>,
    tail: String,
    summary: String,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_graph(steps: &[StepParams]) -> Vec<String> {
    let mut errors = Vec::new();
    if steps.is_empty() {
        errors.push("a workflow needs at least one step".into());
    }
    let mut seen = HashSet::new();
    for s in steps {
        if !valid_name(&s.id) {
            errors.push(format!("step id `{}` must be 1-40 letters, digits, `-` or `_`", s.id));
        } else if !seen.insert(s.id.as_str()) {
            errors.push(format!("duplicate step id `{}`", s.id));
        }
    }
    for s in steps {
        for d in s.depends_on.iter().flatten() {
            if d == &s.id {
                errors.push(format!("step `{}` depends on itself", s.id));
            } else if !seen.contains(d.as_str()) {
                errors.push(format!("step `{}` depends on unknown step `{d}`", s.id));
            }
        }
    }
    if errors.is_empty() {
        let mut done: HashSet<&str> = HashSet::new();
        while done.len() < steps.len() {
            let ready: Vec<&str> = steps
                .iter()
                .filter(|s| !done.contains(s.id.as_str()) && s.depends_on.iter().flatten().all(|d| done.contains(d.as_str())))
                .map(|s| s.id.as_str())
                .collect();
            if ready.is_empty() {
                let cyc: Vec<&str> = steps.iter().filter(|s| !done.contains(s.id.as_str())).map(|s| s.id.as_str()).collect();
                errors.push(format!("dependency cycle among steps: {}", cyc.join(", ")));
                break;
            }
            done.extend(ready);
        }
    }
    errors
}

pub(crate) async fn plan(server: &SidekickServer, snap: &Snapshot, p: WorkflowParams) -> Result<Plan, String> {
    let errors = check_graph(&p.steps);
    if !errors.is_empty() {
        return Err(format!("workflow rejected: {}", errors.join("; ")));
    }
    let id = match p.workflow_id.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) if valid_name(w) => w.to_string(),
        Some(w) => return Err(format!("workflow_id `{w}` must be 1-40 letters, digits, `-` or `_`")),
        None => format!("w-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
    };
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    for s in &p.steps {
        let dp = DispatchParams {
            prompt: s.prompt.clone(),
            cwd: p.cwd.clone(),
            dimension: s.dimension.clone(),
            scope: s.scope.clone(),
            backend: s.backend.clone(),
            model: s.model.clone(),
            job_id: Some(format!("{id}-{}", s.id)),
            max_turns: s.max_turns,
            timeout_s: s.timeout_s,
            ..Default::default()
        };
        let checked = async {
            let prep = server.prepare_dispatch(snap, &dp).await?;
            if !server.registry_gate(snap, &prep.backend, prep.dreq.model.as_deref()).await? {
                return Err(format!("backend {} with this model is not registered (no models.parquet row); register it first", prep.backend.name));
            }
            Ok(prep)
        };
        match checked.await {
            Ok(prep) => steps.push(Planned { id: s.id.clone(), depends_on: s.depends_on.clone().unwrap_or_default(), dimension: prep.dreq.dimension, prep }),
            Err(e) => errors.push(format!("step `{}`: {e}", s.id)),
        }
    }
    if !errors.is_empty() {
        return Err(format!("workflow rejected, nothing ran:\n{}", errors.join("\n")));
    }
    let repo = steps[0].prep.dreq.repo.clone();
    git(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|e| format!("workflow rejected: cwd is not a git repository with a commit: {e:#}"))?;
    Ok(Plan { id, repo, steps })
}

fn prefixed_prompt(prompt: &str, deps: &[&StepResult]) -> String {
    if deps.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from("Results of the steps this task builds on:\n");
    for d in deps {
        let _ = write!(out, "\n## step {} ({})\n{}\n", d.id, d.status, d.tail);
        if !d.summary.is_empty() {
            let _ = write!(out, "patch summary:\n{}\n", d.summary);
        }
    }
    format!("{out}\n---\n\n{prompt}")
}

fn merge(wt: &std::path::Path, branch: &str) -> Result<String> {
    let summary = git(wt, &["show", "--stat", "--format=", branch])?;
    let mut args: Vec<&str> = GIT_IDENT.to_vec();
    args.extend(["cherry-pick", branch]);
    if let Err(e) = git(wt, &args) {
        let _ = git(wt, &["cherry-pick", "--abort"]);
        return Err(anyhow!("merge into the workflow branch failed: {e:#}"));
    }
    Ok(summary.trim().to_string())
}

fn step_result(planned: &Planned, report: &JobReport, wt: &std::path::Path, tip: &mut String) -> StepResult {
    let mut r = StepResult {
        id: planned.id.clone(),
        job_id: planned.prep.dreq.job_id.clone(),
        status: "Failed".into(),
        ok: false,
        writing: planned.dimension.is_write(),
        gold: false,
        merged: false,
        reason: None,
        model_id: None,
        tail: String::new(),
        summary: String::new(),
    };
    let Some(res) = &report.result else {
        r.reason = Some(report.text.clone());
        return r;
    };
    r.status = format!("{:?}", res.status);
    r.gold = res.gold;
    r.model_id = Some(res.model_id.clone());
    r.tail = crate::runner::truncate_tail(&res.tail, TAIL_PER_STEP);
    r.reason = res.block_reason.clone().or_else(|| res.reason.clone());
    r.ok = matches!(res.status, Status::Completed | Status::NoChange);
    if let (true, Some(branch)) = (res.status == Status::Completed, &res.branch) {
        match merge(wt, branch) {
            Ok(summary) => {
                r.merged = true;
                r.summary = summary;
                if let Ok(head) = git(wt, &["rev-parse", "HEAD"]) {
                    *tip = head.trim().to_string();
                }
                let _ = git(&planned.prep.dreq.repo, &["branch", "-D", branch]);
            }
            Err(e) => {
                r.ok = false;
                r.status = "MergeConflict".into();
                r.reason = Some(format!("{e:#}"));
            }
        }
    }
    r
}

fn skipped(planned: &Planned, why: String) -> StepResult {
    StepResult {
        id: planned.id.clone(),
        job_id: planned.prep.dreq.job_id.clone(),
        status: "Skipped".into(),
        ok: false,
        writing: planned.dimension.is_write(),
        gold: false,
        merged: false,
        reason: Some(why),
        model_id: None,
        tail: String::new(),
        summary: String::new(),
    }
}

pub(crate) async fn run(server: SidekickServer, snap: Arc<Snapshot>, plan: Plan, session: Arc<Session>) -> (String, bool) {
    match execute(server, snap, plan, session).await {
        Ok(v) => v,
        Err(e) => (redact_all(&format!("workflow failed: {e:#}")), true),
    }
}

async fn execute(server: SidekickServer, snap: Arc<Snapshot>, plan: Plan, session: Arc<Session>) -> Result<(String, bool)> {
    let Plan { id, repo, steps } = plan;
    let work = steps[0].prep.store.base.join("work");
    let branch = format!("{BRANCH_PREFIX}{id}");
    let base = git(&repo, &["rev-parse", "HEAD"])?.trim().to_string();
    let wt = work.join("worktrees").join(&id);
    std::fs::create_dir_all(wt.parent().unwrap_or(&work))?;
    git(&repo, &["branch", &branch, &base])?;
    if let Err(e) = git(&repo, &["worktree", "add", &wt.to_string_lossy(), &branch]) {
        let _ = git(&repo, &["branch", "-D", &branch]);
        return Err(e);
    }
    let mut tip = base.clone();
    let n = steps.len();
    let mut results: Vec<Option<StepResult>> = (0..n).map(|_| None).collect();
    let mut started = vec![false; n];
    let mut set: JoinSet<(usize, JobReport)> = JoinSet::new();
    loop {
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..n {
                if started[i] {
                    continue;
                }
                let deps: Vec<usize> = steps[i].depends_on.iter().filter_map(|d| steps.iter().position(|s| &s.id == d)).collect();
                if session.cancel.is_cancelled() {
                    started[i] = true;
                    results[i] = Some(skipped(&steps[i], "workflow cancelled".into()));
                    changed = true;
                } else if let Some(bad) = deps.iter().find(|&&d| results[d].as_ref().is_some_and(|r| !r.ok)) {
                    let r = results[*bad].as_ref().expect("checked");
                    started[i] = true;
                    results[i] = Some(skipped(&steps[i], format!("dependency `{}` {}", r.id, r.status.to_lowercase())));
                    changed = true;
                } else if deps.iter().all(|&d| results[d].is_some()) {
                    started[i] = true;
                    let done: Vec<&StepResult> = deps.iter().filter_map(|&d| results[d].as_ref()).collect();
                    let mut prep = steps[i].prep.clone();
                    prep.dreq.prompt = prefixed_prompt(&prep.dreq.prompt, &done);
                    let extras = JobExtras { base: Some(tip.clone()), hold_gold: true, step: Some(steps[i].id.clone()) };
                    let (srv, snap, session) = (server.clone(), snap.clone(), session.clone());
                    set.spawn(async move { (i, srv.execute_job(snap, prep, session, extras).await) });
                }
            }
        }
        let Some(joined) = set.join_next().await else { break };
        let (i, report) = joined.map_err(|e| anyhow!("a step task failed: {e}"))?;
        results[i] = Some(step_result(&steps[i], &report, &wt, &mut tip));
    }
    let results: Vec<StepResult> = results.into_iter().flatten().collect();
    let all_ok = results.iter().all(|r| r.ok);
    let merged_any = results.iter().any(|r| r.merged);
    let gold_only = merged_any && results.iter().filter(|r| r.writing).all(|r| r.ok && (r.gold || !r.merged));
    let patch = if merged_any { git(&repo, &["diff", "--binary", "--no-color", "--no-renames", &base, &tip])? } else { String::new() };
    let patch_path = work.join("patches").join(format!("{id}.patch"));
    let mut applied = false;
    let mut warning = None;
    if merged_any {
        std::fs::create_dir_all(patch_path.parent().unwrap_or(&work))?;
        std::fs::write(&patch_path, &patch)?;
        if gold_only && all_ok {
            match git(&repo, &["apply", "--whitespace=nowarn", &patch_path.to_string_lossy()]) {
                Ok(_) => applied = true,
                Err(e) => warning = Some(format!("gold apply failed, branch and patch kept: {e:#}")),
            }
        }
    }
    let _ = git(&repo, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
    let keep_branch = merged_any && !applied;
    if !keep_branch {
        let _ = git(&repo, &["branch", "-D", &branch]);
    }
    let mut out = format!("status: workflow {}\nworkflow: {id}\nbase: {base}\napplied: {applied}\n", if all_ok { "completed" } else { "partial" });
    if keep_branch {
        let _ = write!(out, "branch: {branch}\npatch: {}\n", patch_path.display());
    } else if !merged_any {
        out.push_str("no step changed files\n");
    }
    if let Some(w) = warning {
        let _ = writeln!(out, "warning: {w}");
    }
    out.push_str("steps:\n");
    for r in &results {
        let _ = write!(out, "- {} job_id={} status={}", r.id, r.job_id, r.status);
        if let Some(m) = &r.model_id {
            let _ = write!(out, " model_id={m} gold={}", r.gold);
        }
        if r.merged {
            out.push_str(" merged=true");
        }
        if let Some(reason) = &r.reason {
            let _ = write!(out, "\n  reason: {}", reason.lines().next().unwrap_or_default());
        }
        out.push('\n');
    }
    for r in results.iter().filter(|r| !r.tail.is_empty()) {
        let _ = write!(out, "--- step {} output ---\n{}\n", r.id, r.tail);
    }
    Ok((redact_all(&out), !all_ok))
}
