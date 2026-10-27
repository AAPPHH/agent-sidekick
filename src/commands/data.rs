use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};
use arrow::datatypes::SchemaRef;

use crate::data::{experience_schema, read_events, read_schema, strip_private, write_events, DataDir, ModelRow, ProtectedRow};
use crate::dispatch::{git, glob_match};

pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const BOT: &str = "github-actions[bot]";
pub const SHARED_RULES: [&str; 3] = ["models.parquet", "protected.parquet", "peers.toml"];
pub const SHARED_RULE_GLOBS: [&str; 1] = [".github/**"];
const PEERS: &str = "peers.toml";
const ZERO_REV: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, clap::Args)]
pub struct DataArgs {
    #[command(subcommand)]
    pub command: DataCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum DataCommand {
    /// Check the changes between two revisions against the pusher's prefixes and the data rules.
    Validate {
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: String,
        #[arg(long)]
        pusher: String,
        #[arg(long)]
        root: PathBuf,
        /// Write the violating paths (one per line) here once validation has run.
        #[arg(long)]
        paths_out: Option<PathBuf>,
    },
    /// Union and dedup all experiences into aggregate/latest.parquet.
    Aggregate {
        #[arg(long)]
        root: PathBuf,
    },
}

pub fn run(args: DataArgs) -> Result<ExitCode> {
    match args.command {
        DataCommand::Validate { base, head, pusher, root, paths_out } => {
            let found = Validator { root: &root, pusher: &pusher }.violations(&base, &head)?;
            if let Some(out) = paths_out {
                let mut paths: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
                paths.dedup();
                std::fs::write(out, paths.iter().map(|p| format!("{p}\n")).collect::<String>())?;
            }
            let violations: Vec<String> = found.into_iter().map(|(p, m)| format!("{p}: {m}")).collect();
            if violations.is_empty() {
                return Ok(ExitCode::SUCCESS);
            }
            println!("{} violation(s):", violations.len());
            violations.iter().for_each(|v| println!("- {v}"));
            Ok(ExitCode::FAILURE)
        }
        DataCommand::Aggregate { root } => {
            let n = aggregate(&root)?;
            println!("{n} events");
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn aggregate(root: &Path) -> Result<usize> {
    let data = DataDir::new(root.to_path_buf());
    let events: Vec<_> = data.experience_events()?.iter().map(strip_private).collect();
    let path = data.aggregate_path();
    if !path.exists() || read_events(&path)? != events {
        write_events(&path, &events)?;
    }
    Ok(events.len())
}

fn is_pseudonym(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub struct Validator<'a> {
    pub root: &'a Path,
    pub pusher: &'a str,
}

type Violation = (String, String);

fn git_z(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let mut full = vec!["-c", "core.quotepath=false"];
    full.extend(args);
    Ok(git(root, &full)?.split('\0').filter(|p| !p.is_empty()).map(String::from).collect())
}

fn shared_rule(path: &str) -> bool {
    SHARED_RULES.contains(&path) || SHARED_RULE_GLOBS.iter().any(|g| glob_match(g, path))
}

fn root_path(path: &str) -> bool {
    !path.starts_with("experiences/") && !path.starts_with("aggregate/")
}

fn prefix_of(path: &str) -> Option<&str> {
    path.strip_prefix("experiences/")?.split_once('/').map(|(p, _)| p)
}

impl Validator<'_> {
    pub fn validate(&self, base: &str, head: &str) -> Result<Vec<String>> {
        Ok(self.violations(base, head)?.into_iter().map(|(p, m)| format!("{p}: {m}")).collect())
    }

    pub fn violations(&self, base: &str, head: &str) -> Result<Vec<Violation>> {
        let zero = base == ZERO_REV;
        let listing = if zero {
            git_z(self.root, &["ls-tree", "-r", "--name-only", "-z", head])?
        } else {
            git_z(self.root, &["diff", "--name-only", "-z", "--no-renames", base, head])?
        };
        let changed: Vec<String> = listing.into_iter().filter(|p| !zero || !p.starts_with(".github/")).collect();
        let (prefixes, owners) = self.peers(base)?;
        let is_owner = owners.iter().any(|o| o.eq_ignore_ascii_case(self.pusher));
        let mut foreign: Option<Vec<(String, String)>> = None;
        let mut out = Vec::new();
        for path in &changed {
            if root_path(path) {
                if !is_owner {
                    let m = if shared_rule(path) { format!("shared rule file, only the owners may change it (pusher `{}`)", self.pusher) } else { self.outside(&prefixes) };
                    out.push((path.clone(), m));
                }
            } else if let Some(v) = self.path_violation(path, &prefixes) {
                out.push((path.clone(), v));
            }
            let full = self.root.join(path);
            if !full.is_file() {
                continue;
            }
            out.extend(self.content_violations(path, &full).into_iter().map(|m| (path.clone(), m)));
            if let Some(mine) = prefix_of(path).filter(|_| path.ends_with(".parquet")) {
                if foreign.is_none() {
                    foreign = Some(self.base_event_ids(if zero { head } else { base })?);
                }
                let ids: Vec<&(String, String)> = foreign.iter().flatten().filter(|(_, other)| other != mine).collect();
                if let Ok(events) = read_events(&full) {
                    for e in events {
                        if let Some((_, other)) = ids.iter().find(|(id, _)| *id == e.event_id) {
                            out.push((path.clone(), format!("event {}: event_id already exists under prefix `{other}`", e.event_id)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn reset_allowed(&self, rel: &str) -> bool {
        let Some(prefix) = prefix_of(rel) else { return rel.starts_with("aggregate/") };
        let text = std::fs::read_to_string(self.root.join(PEERS)).unwrap_or_default();
        text.parse::<toml::Table>().is_ok_and(|t| crate::sync::peers_owner(&t, prefix))
    }

    fn base_event_ids(&self, rev: &str) -> Result<Vec<(String, String)>> {
        let mut ids = Vec::new();
        let files = git_z(self.root, &["ls-tree", "-r", "--name-only", "-z", rev, "--", "experiences"])?;
        for (n, file) in files.iter().filter(|f| f.ends_with(".parquet")).enumerate() {
            let Some(prefix) = prefix_of(file) else { continue };
            let blob = Command::new("git").arg("-C").arg(self.root).args(["cat-file", "blob", &format!("{rev}:{file}")]).output()?;
            if !blob.status.success() {
                continue;
            }
            let tmp = std::env::temp_dir().join(format!("sidekick-validate-{}-{n}.parquet", std::process::id()));
            std::fs::write(&tmp, &blob.stdout)?;
            let events = read_events(&tmp);
            let _ = std::fs::remove_file(&tmp);
            if let Ok(events) = events {
                ids.extend(events.into_iter().map(|e| (e.event_id, prefix.to_string())));
            }
        }
        Ok(ids)
    }

    fn peers(&self, base: &str) -> Result<(Vec<String>, Vec<String>)> {
        let text = if base == ZERO_REV {
            std::fs::read_to_string(self.root.join(PEERS)).unwrap_or_default()
        } else {
            git(self.root, &["show", &format!("{base}:{PEERS}")]).unwrap_or_default()
        };
        let table: toml::Table = text.parse().with_context(|| format!("parse {PEERS}"))?;
        let owners = table
            .get("owners")
            .and_then(|o| o.as_array())
            .map(|a| a.iter().filter_map(|o| o.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let prefixes = table
            .iter()
            .filter(|(_, owner)| owner.as_str().is_some_and(|o| o.eq_ignore_ascii_case(self.pusher)))
            .map(|(prefix, _)| format!("experiences/{prefix}/"))
            .collect();
        Ok((prefixes, owners))
    }

    fn path_violation(&self, path: &str, prefixes: &[String]) -> Option<String> {
        if prefixes.iter().any(|p| path.starts_with(p.as_str()) && !path[p.len()..].is_empty()) {
            return None;
        }
        Some(self.outside(prefixes))
    }

    fn outside(&self, prefixes: &[String]) -> String {
        let owned = if prefixes.is_empty() { "no prefix".to_string() } else { prefixes.join(", ") };
        format!("outside the prefixes of `{}` ({owned})", self.pusher)
    }

    fn content_violations(&self, rel: &str, full: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let size = std::fs::metadata(full).map(|m| m.len()).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            out.push(format!("{size} bytes exceeds the limit of {MAX_FILE_BYTES}"));
        }
        if full.extension().is_none_or(|e| e != "parquet") {
            return out;
        }
        let name = rel;
        let result = if name == "models.parquet" {
            check_schema(full, ModelRow::schema()).and_then(|_| ModelRow::read_all(full).map(|_| Vec::new()))
        } else if name == "protected.parquet" {
            check_schema(full, ProtectedRow::schema()).and_then(|_| ProtectedRow::read_all(full).map(|_| Vec::new()))
        } else if name.starts_with("experiences/") || name.starts_with("aggregate/") {
            let reset_ok = self.reset_allowed(rel);
            check_schema(full, experience_schema()).and_then(|_| event_violations(full, reset_ok))
        } else {
            Ok(Vec::new())
        };
        match result {
            Ok(v) => out.extend(v),
            Err(e) => out.push(format!("invalid ({e:#})")),
        }
        out
    }
}

fn check_schema(path: &Path, expected: SchemaRef) -> Result<()> {
    let found = read_schema(path)?;
    let sig = |s: &SchemaRef| s.fields().iter().map(|f| (f.name().clone(), f.data_type().clone())).collect::<Vec<_>>();
    let (found_sig, expected_sig) = (sig(&found), sig(&expected));
    let appended_nullable = expected.fields().iter().skip(found_sig.len()).all(|f| f.is_nullable());
    if found_sig.len() > expected_sig.len() || expected_sig[..found_sig.len()] != found_sig[..] || !appended_nullable {
        return Err(anyhow!("schema does not match the expected columns"));
    }
    Ok(())
}

fn event_violations(path: &Path, reset_ok: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in read_events(path)? {
        let id = &e.event_id;
        if e.visibility == crate::trust::Visibility::Private && (e.summary.is_some() || e.conclusion.is_some()) {
            out.push(format!("event {id}: summary/conclusion text on a private row"));
        }
        for (field, value) in [("user_id", &e.user_id), ("machine_id", &e.machine_id)] {
            if !is_pseudonym(value) {
                out.push(format!("event {id}: {field} is not a 16-hex pseudonym"));
            }
        }
        if e.outcome == crate::trust::Outcome::Seed && !e.dimension.seedable() {
            out.push(format!("event {id}: seed for dimension {} (only search/inventory)", e.dimension));
        }
        if e.outcome == crate::trust::Outcome::Reset && !reset_ok {
            out.push(format!("event {id}: reset row from a prefix whose owner is not listed in peers.toml owners"));
        }
    }
    Ok(out)
}
