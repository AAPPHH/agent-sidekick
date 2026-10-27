use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};

use crate::data::{DataDir, SessionLog};
use crate::dispatch::{git, GIT_IDENT};
use crate::trust::{Dimension, Event, Outcome, Scope, Visibility};

pub const BRANCH: &str = "main";
pub const COMMIT_PREFIX: &str = "data: ";
pub const PUSH_ATTEMPTS: usize = 3;
const REMOTE: &str = "origin";

#[derive(Clone, Debug)]
pub struct ExperienceInput {
    pub model_id: String,
    pub dimension: Dimension,
    pub scope: Scope,
    pub outcome: Outcome,
    pub job_id: Option<String>,
    pub steps: Option<i64>,
    pub duration_s: Option<f64>,
    pub review_s: Option<f64>,
    pub seed_value: Option<f64>,
    pub summary: Option<String>,
    pub conclusion: Option<String>,
    pub visibility: Visibility,
}

#[derive(Debug)]
pub struct DataStore {
    pub base: PathBuf,
    pub checkout: PathBuf,
    pub local_root: PathBuf,
    pub data: DataDir,
    pub name: String,
    pub push_enabled: bool,
    pub machine_id: String,
    pub user_id: String,
    log: Mutex<SessionLog>,
    git_lock: Mutex<()>,
}

fn read_or_create(path: &Path, make: impl FnOnce() -> String) -> Result<String> {
    if let Ok(v) = fs::read_to_string(path) {
        if !v.trim().is_empty() {
            return Ok(v.trim().to_string());
        }
    }
    let v = make();
    fs::write(path, &v).with_context(|| format!("write {}", path.display()))?;
    Ok(v)
}

fn add_remote(checkout: &Path, remote: &str) -> Result<()> {
    git(checkout, &["remote", "add", REMOTE, remote]).map(|_| ()).or_else(|e| match git(checkout, &["remote", "get-url", REMOTE]) {
        Ok(u) if u.trim() == remote => Ok(()),
        _ => Err(e),
    })
}

pub fn remote_url(repo: &str) -> String {
    let slug = repo.split_once('/').is_some_and(|(o, n)| {
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        ok(o) && ok(n)
    });
    if slug && !Path::new(repo).exists() {
        format!("https://github.com/{repo}.git")
    } else {
        repo.to_string()
    }
}

pub fn peers_check(table: &toml::Table) -> Result<()> {
    let mut seen = std::collections::BTreeMap::new();
    for key in table.keys() {
        if let Some(other) = seen.insert(key.to_ascii_lowercase(), key) {
            bail!("peers.toml: keys `{other}` and `{key}` differ only in case");
        }
    }
    Ok(())
}

pub fn peers_owner(table: &toml::Table, prefix: &str) -> bool {
    let owner = table.get(prefix).filter(|_| prefix != "owners").and_then(|v| v.as_str());
    let owners = table.get("owners").and_then(|o| o.as_array());
    peers_check(table).is_ok() && matches!((owner, owners), (Some(o), Some(a)) if a.iter().filter_map(|x| x.as_str()).any(|x| x.eq_ignore_ascii_case(o)))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl DataStore {
    pub fn checkout_exists(base: &Path) -> bool {
        base.join("checkout").join(".git").exists()
    }

    pub fn open(base: &Path, repo: &str, name: Option<&str>) -> Result<Self> {
        let name_ok = name.filter(|n| valid_name(n));
        if let Some(n) = name {
            if name_ok.is_none() {
                bail!("invalid data_name `{n}`");
            }
        }
        let remote = &remote_url(repo);
        let checkout = base.join("checkout");
        let local_root = base.join("local");
        fs::create_dir_all(&checkout)?;
        fs::create_dir_all(&local_root)?;
        if !checkout.join(".git").exists() {
            git(&checkout, &["init", "-q", "-b", BRANCH])?;
            add_remote(&checkout, remote)?;
        } else if git(&checkout, &["remote", "get-url", REMOTE]).map(|u| u.trim() != remote).unwrap_or(true) {
            git(&checkout, &["remote", "set-url", REMOTE, remote]).map(|_| ()).or_else(|_| add_remote(&checkout, remote))?;
        }
        let secret = read_or_create(&base.join("secret"), || {
            format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
        })?;
        let machine_id = read_or_create(&base.join("machine_id"), || uuid::Uuid::new_v4().to_string())?;
        let user_id = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "unknown".into());
        let own = name_ok.unwrap_or("local");
        let data = DataDir::new(checkout.clone());
        let log = SessionLog::open(&data.root, &local_root, own, &uuid::Uuid::new_v4().to_string())?.with_secret(secret.into_bytes());
        Ok(Self {
            base: base.to_path_buf(),
            checkout,
            local_root,
            data,
            name: own.to_string(),
            push_enabled: name_ok.is_some(),
            machine_id,
            user_id,
            log: Mutex::new(log),
            git_lock: Mutex::new(()),
        })
    }

    fn pending_path(&self) -> PathBuf {
        self.base.join("pending")
    }

    pub fn pending(&self) -> bool {
        self.pending_path().exists()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.git_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn own_prefix(&self) -> PathBuf {
        Path::new("experiences").join(&self.name)
    }

    pub fn pull(&self) -> Result<()> {
        let _g = self.lock();
        self.pull_locked()
    }

    fn pull_locked(&self) -> Result<()> {
        let r = git(&self.checkout, &["pull", "--rebase", "--autostash", "-q", REMOTE, BRANCH]);
        if r.is_err() {
            let _ = git(&self.checkout, &["rebase", "--abort"]);
        }
        r.map(|_| ())
    }

    fn push_locked(&self) -> bool {
        for _ in 0..PUSH_ATTEMPTS {
            if git(&self.checkout, &["push", "-q", REMOTE, &format!("HEAD:{BRANCH}")]).is_ok() {
                let _ = fs::remove_file(self.pending_path());
                return true;
            }
            if self.pull_locked().is_err() {
                break;
            }
        }
        let _ = fs::write(self.pending_path(), "1");
        false
    }

    fn commit_own(&self, rel: &Path, message: &str) -> Result<()> {
        if !rel.starts_with(self.own_prefix()) {
            bail!("refusing to commit {} outside {}", rel.display(), self.own_prefix().display());
        }
        let rel = rel.to_string_lossy();
        git(&self.checkout, &["add", "--", &rel])?;
        if git(&self.checkout, &["status", "--porcelain", "--", &rel])?.trim().is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = GIT_IDENT.to_vec();
        args.extend(["commit", "-q", "-m", message, "--", &rel]);
        git(&self.checkout, &args).map(|_| ())
    }

    pub fn append(&self, event: Event) -> Result<(String, bool)> {
        let id = event.event_id.clone();
        let (shared, session) = {
            let mut log = self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            log.append(event)?;
            let session = log.shared_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            (log.shared_path.clone(), session)
        };
        if !self.push_enabled {
            return Ok((id, false));
        }
        let _g = self.lock();
        let rel = shared.strip_prefix(&self.checkout).map_err(|_| anyhow!("session file outside the checkout"))?;
        self.commit_own(rel, &format!("{COMMIT_PREFIX}{} {session} {id}", self.name))?;
        Ok((id, self.push_locked()))
    }

    pub fn is_owner(&self) -> bool {
        let Ok(text) = fs::read_to_string(self.checkout.join("peers.toml")) else { return false };
        let Ok(table) = text.parse::<toml::Table>() else { return false };
        peers_owner(&table, &self.name)
    }

    pub fn record(&self, input: ExperienceInput) -> Result<(String, bool)> {
        match input.outcome {
            Outcome::Blocked => bail!("outcome blocked is written by the sidekick after the diff check, not by record_experience"),
            Outcome::Seed => {
                if !input.dimension.seedable() {
                    bail!("seed is only allowed for search and inventory, not {}", input.dimension);
                }
                match input.seed_value {
                    Some(v) if (0.0..=1.0).contains(&v) => {}
                    _ => bail!("seed needs seed_value between 0 and 1"),
                }
            }
            Outcome::Reset => {
                if input.job_id.is_some() {
                    bail!("reset takes no job_id");
                }
                if !self.is_owner() {
                    bail!("reset is only allowed for owners listed in peers.toml (prefix `{}`)", self.name);
                }
            }
            _ => {}
        }
        if input.review_s.is_some_and(|v| v < 0.0) || input.duration_s.is_some_and(|v| v < 0.0) {
            bail!("review_s and duration_s must not be negative");
        }
        self.data.model(&input.model_id)?;
        let scope = if input.outcome == Outcome::Reset { Scope::Global } else { input.scope };
        let mut e = Event::new(&input.model_id, input.dimension, scope, input.outcome, chrono::Utc::now().timestamp_millis());
        e.machine_id = self.machine_id.clone();
        e.user_id = self.user_id.clone();
        e.job_id = input.job_id;
        e.steps = input.steps;
        e.duration_s = input.duration_s;
        e.review_s = input.review_s;
        e.seed_value = (input.outcome == Outcome::Seed).then_some(input.seed_value).flatten();
        e.summary = input.summary;
        e.conclusion = input.conclusion;
        e.visibility = input.visibility;
        self.append(e)
    }

    pub fn record_verdict(&self, job_id: &str, outcome: Outcome, conclusion: Option<String>, visibility: Visibility) -> Result<(String, bool)> {
        if !matches!(outcome, Outcome::Accepted | Outcome::Rejected) {
            bail!("a verdict on a job is accepted or rejected");
        }
        let base = {
            let log = self.log.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mine: Vec<&Event> = log.events().iter().filter(|e| e.job_id.as_deref() == Some(job_id)).collect();
            if mine.iter().any(|e| e.outcome == Outcome::Blocked) {
                bail!("job {job_id} was blocked; blocked jobs get no verdict");
            }
            if mine.iter().any(|e| matches!(e.outcome, Outcome::Accepted | Outcome::Rejected)) {
                bail!("job {job_id} already has a verdict");
            }
            mine.into_iter().find(|e| matches!(e.outcome, Outcome::Note | Outcome::Aborted)).cloned()
        };
        let base = base.ok_or_else(|| anyhow!("unknown job_id {job_id} in the local log of this session"))?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut e = Event::new(&base.model_id, base.dimension, base.scope.clone(), outcome, now);
        e.machine_id = self.machine_id.clone();
        e.user_id = self.user_id.clone();
        e.repo = base.repo.clone();
        e.job_id = base.job_id.clone();
        e.steps = base.steps;
        e.duration_s = base.duration_s;
        e.review_s = Some(((now - base.ts_ms).max(0) as f64) / 1000.0);
        e.conclusion = conclusion;
        e.visibility = visibility;
        self.append(e)
    }

    pub fn sync(&self) -> Result<String> {
        let _g = self.lock();
        let pulled = match self.pull_locked() {
            Ok(()) => "pull: ok".to_string(),
            Err(e) => format!("pull failed: {e:#}"),
        };
        if !self.push_enabled {
            return Ok(format!("{pulled}; push disabled (data_name not set)"));
        }
        let own = self.own_prefix();
        let dirty = !git(&self.checkout, &["status", "--porcelain", "--", &own.to_string_lossy()])?.trim().is_empty();
        if dirty {
            self.commit_own(&own, &format!("{COMMIT_PREFIX}{} sync", self.name))?;
        }
        let remote_ref = format!("{REMOTE}/{BRANCH}");
        let has_head = git(&self.checkout, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
        let ahead = match git(&self.checkout, &["rev-list", "--count", &format!("{remote_ref}..HEAD")]) {
            Ok(n) => n.trim().parse::<usize>().unwrap_or(0),
            Err(_) => usize::from(has_head),
        };
        let push = if ahead == 0 && !self.pending() {
            "nothing to push"
        } else if self.push_locked() {
            "pushed (private summary/conclusion stay local)"
        } else {
            "push pending (offline or rejected)"
        };
        Ok(format!("{pulled}; {push}"))
    }
}
