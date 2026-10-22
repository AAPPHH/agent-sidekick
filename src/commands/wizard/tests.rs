//! The wizard with scripted answers and a fake machine: no real config dir, keyring, PATH,
//! npm or Claude Code registration is touched.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use super::npm::Invocation;
use super::system::{System, Tool};
use super::*;
use crate::backends::{reach, ResolveDefaults};

// ----- scripted prompter -----

#[derive(Debug)]
enum A {
    Yes,
    No,
    One(usize),
    Many(Vec<usize>),
    Secret(&'static str),
}

struct Script {
    answers: VecDeque<A>,
    out: String,
}

impl Script {
    fn new(answers: Vec<A>) -> Self {
        Self {
            answers: answers.into(),
            out: String::new(),
        }
    }

    fn next(&mut self, question: &str) -> A {
        self.answers
            .pop_front()
            .unwrap_or_else(|| panic!("no scripted answer left for: {question}\n{}", self.out))
    }

    fn assert_done(&self) {
        assert!(
            self.answers.is_empty(),
            "unused answers {:?}\n{}",
            self.answers,
            self.out
        );
    }
}

impl Prompter for Script {
    fn say(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn confirm(&mut self, question: &str, _default: bool) -> Result<bool> {
        self.out.push_str(&format!("? {question}\n"));
        match self.next(question) {
            A::Yes => Ok(true),
            A::No => Ok(false),
            other => panic!("expected yes/no for {question}, got {other:?}"),
        }
    }

    fn choose_one(&mut self, question: &str, options: &[String], default: usize) -> Result<usize> {
        self.out.push_str(&format!(
            "? {question} {} default {default}\n",
            options.join(" | ")
        ));
        match self.next(question) {
            A::One(i) => {
                assert!(i < options.len());
                Ok(i)
            }
            other => panic!("expected one choice for {question}, got {other:?}"),
        }
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        defaults: &[usize],
    ) -> Result<Vec<usize>> {
        self.out.push_str(&format!(
            "? {question} {} default {defaults:?}\n",
            options.join(" | ")
        ));
        match self.next(question) {
            A::Many(v) => Ok(v),
            other => panic!("expected a multi choice for {question}, got {other:?}"),
        }
    }

    fn secret(&mut self, prompt: &str) -> Result<Token> {
        self.out.push_str(&format!("? {prompt}\n"));
        match self.next(prompt) {
            A::Secret(s) => Ok(Token::new(s)),
            other => panic!("expected a secret for {prompt}, got {other:?}"),
        }
    }
}

// ----- fake machine -----

type ProbeFn = Box<dyn Fn(&str, Option<&str>) -> ProbeResponse>;

struct Fake {
    os: Os,
    dir: tempfile::TempDir,
    claude: Option<PathBuf>,
    node: Option<PathBuf>,
    opencode: RefCell<Option<PathBuf>>,
    gemini: RefCell<Option<PathBuf>>,
    npm: Option<Invocation>,
    install_ok: bool,
    runs: RefCell<Vec<Invocation>>,
    secrets: RefCell<HashMap<SecretId, String>>,
    probe: ProbeFn,
    probes: RefCell<Vec<(String, Option<String>)>>,
    register_ok: bool,
    registered: Cell<u32>,
    doctor_runs: Cell<u32>,
    codex: RefCell<Option<PathBuf>>,
    codex_registered: Cell<u32>,
}

const CLAUDE_TOKEN: &str = "sk-ant-oat01-SECRETclaudeTOKEN123";
const KIC_KEY: &str = "kic-SECRET-key-0123456789";
const KIC_KEY2: &str = "kic-SECRET-key-second-999";
const GEMINI_KEY: &str = "AIzaSECRETgeminiKEY42";
const GROQ_KEY: &str = "gsk_SECRET-groq-key-0123456789";

impl Fake {
    /// Everything installed, npm available, the provider accepts every key.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Fake {
            os: Os::current(),
            claude: Some(root.join("bin").join("claude.exe")),
            node: Some(root.join("nodejs").join("node.exe")),
            opencode: RefCell::new(Some(root.join("npm").join("opencode.exe"))),
            gemini: RefCell::new(Some(
                root.join("npm")
                    .join("node_modules")
                    .join("@google")
                    .join("gemini-cli")
                    .join("bundle")
                    .join("gemini.js"),
            )),
            npm: Some(Invocation {
                program: root.join("nodejs").join("node.exe"),
                args: vec![root
                    .join("nodejs")
                    .join("node_modules")
                    .join("npm")
                    .join("bin")
                    .join("npm-cli.js")
                    .into_os_string()],
            }),
            install_ok: true,
            runs: RefCell::new(Vec::new()),
            secrets: RefCell::new(HashMap::new()),
            probe: Box::new(|url, _| ProbeResponse::Status {
                code: 200,
                url: url.to_owned(),
                millis: 1,
            }),
            probes: RefCell::new(Vec::new()),
            register_ok: true,
            registered: Cell::new(0),
            doctor_runs: Cell::new(0),
            // Not installed by default, so the Codex question appears only where tested.
            codex: RefCell::new(None),
            codex_registered: Cell::new(0),
            dir,
        }
    }

    fn cfg(&self) -> PathBuf {
        self.config_file().unwrap()
    }

    fn written(&self) -> String {
        std::fs::read_to_string(self.cfg()).unwrap()
    }

    fn resolved(&self) -> Backends {
        templates::validate(&self.written(), &self.resolve_defaults())
            .unwrap()
            .1
    }

    fn secret(&self, id: &SecretId) -> Option<String> {
        self.secrets.borrow().get(id).cloned()
    }

    fn backups(&self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(self.cfg().parent().unwrap()) else {
            return Vec::new();
        };
        let mut v: Vec<PathBuf> = rd
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".bak-"))
            .collect();
        v.sort();
        v
    }

    fn seed_config(&self, text: &str) {
        std::fs::create_dir_all(self.cfg().parent().unwrap()).unwrap();
        std::fs::write(self.cfg(), text).unwrap();
    }
}

impl System for Fake {
    fn os(&self) -> Os {
        self.os
    }
    fn config_file(&self) -> Result<PathBuf> {
        Ok(self.dir.path().join("cfg").join("config.toml"))
    }
    fn resolve_defaults(&self) -> ResolveDefaults {
        let home = self.dir.path().join("home");
        ResolveDefaults {
            legacy_agent_config_dir: Some(home.join(".claude-sidekick")),
            home: Some(home),
            config_dir: Some(self.dir.path().join("cfg")),
        }
    }
    fn find_claude(&self, _claude_path: Option<&Path>) -> Result<PathBuf, String> {
        self.claude.clone().ok_or_else(|| "claude not found".into())
    }
    fn find_node(&self) -> Option<PathBuf> {
        self.node.clone()
    }
    fn find_opencode(&self) -> Option<PathBuf> {
        self.opencode.borrow().clone()
    }
    fn npm(&self, _node: &Path) -> Option<Invocation> {
        self.npm.clone()
    }
    fn find_gemini(&self, _node: &Path, _npm: Option<&Invocation>) -> Option<PathBuf> {
        self.gemini.borrow().clone()
    }
    fn version(&self, tool: Tool, _path: &Path) -> Result<String, String> {
        Ok(match tool {
            Tool::Claude => "2.1.296 (Claude Code)",
            Tool::Node => "v22.11.0",
            Tool::OpenCode => "1.14.0",
            Tool::Codex => "codex-cli 0.163.0",
        }
        .to_owned())
    }
    fn run_visible(&self, inv: &Invocation) -> Result<bool> {
        self.runs.borrow_mut().push(inv.clone());
        if self.install_ok {
            let args: Vec<String> = inv
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let root = self.dir.path().join("installed");
            if args.iter().any(|a| a == npm::OPENCODE_PACKAGE) {
                *self.opencode.borrow_mut() = Some(root.join("opencode.exe"));
            }
            if args.iter().any(|a| a == npm::CODEX_PACKAGE) {
                *self.codex.borrow_mut() = Some(root.join("codex.exe"));
            }
            if args.iter().any(|a| a == npm::GEMINI_PACKAGE) {
                *self.gemini.borrow_mut() = Some(root.join("gemini.js"));
            }
        }
        Ok(self.install_ok)
    }
    fn stored_secret(&self, id: &SecretId) -> Option<String> {
        self.secrets
            .borrow()
            .contains_key(id)
            .then(|| "im Fake-Schlüsselbund".to_owned())
    }
    fn store_secret(&self, id: &SecretId, secret: &Token) -> Result<String> {
        self.secrets
            .borrow_mut()
            .insert(id.clone(), secret.expose().to_owned());
        Ok("im Fake-Schlüsselbund".to_owned())
    }
    fn probe_get(&self, url: &str, bearer: Option<&str>) -> ProbeResponse {
        self.probes
            .borrow_mut()
            .push((url.to_owned(), bearer.map(str::to_owned)));
        (self.probe)(url, bearer)
    }
    fn create_dir(&self, dir: &Path) -> Result<()> {
        assert!(
            dir.starts_with(self.dir.path()),
            "outside the test dir: {}",
            dir.display()
        );
        std::fs::create_dir_all(dir)?;
        Ok(())
    }
    fn register(&self, _claude: &Path) -> Result<()> {
        self.registered.set(self.registered.get() + 1);
        if self.register_ok {
            Ok(())
        } else {
            anyhow::bail!("claude mcp add failed")
        }
    }
    fn find_codex(&self) -> Option<PathBuf> {
        self.codex.borrow().clone()
    }
    fn register_codex(&self, _claude: Option<&Path>) -> Result<()> {
        self.codex_registered.set(self.codex_registered.get() + 1);
        Ok(())
    }
    fn prepare_codex_home(&self, backend: &Backend) -> Result<PathBuf, String> {
        let file = crate::harness::codex::prepare_home(backend)?;
        assert!(
            file.starts_with(self.dir.path()),
            "outside the test dir: {}",
            file.display()
        );
        Ok(file.parent().unwrap().to_path_buf())
    }
    fn file_exists(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn doctor(&self) -> String {
        self.doctor_runs.set(self.doctor_runs.get() + 1);
        "[OK]   FAKE DOCTOR REPORT\n".to_owned()
    }
    fn timestamp(&self) -> String {
        "20261021-120000".to_owned()
    }
}

/// No secret value ever appears in the wizard's output or error.
fn assert_no_secrets(text: &str) {
    for s in [
        CLAUDE_TOKEN,
        KIC_KEY,
        KIC_KEY2,
        GEMINI_KEY,
        GROQ_KEY,
        "SECRET",
    ] {
        assert!(!text.contains(s), "secret {s} printed:\n{text}");
    }
}

fn run_ok(fake: &Fake, answers: Vec<A>) -> (Outcome, String) {
    let mut script = Script::new(answers);
    let outcome = run(&mut script, fake).unwrap_or_else(|e| panic!("{e:#}\n{}", script.out));
    script.assert_done();
    assert_no_secrets(&script.out);
    (outcome, script.out)
}

fn run_err(fake: &Fake, answers: Vec<A>) -> (String, String) {
    let mut script = Script::new(answers);
    let err = run(&mut script, fake).expect_err("the wizard should fail");
    script.assert_done();
    let err = format!("{err:#}");
    assert_no_secrets(&script.out);
    assert_no_secrets(&err);
    (err, script.out)
}

fn assert_nothing_written(fake: &Fake) {
    assert!(!fake.cfg().exists(), "config written");
    assert!(fake.secrets.borrow().is_empty(), "secret stored");
    assert_eq!(fake.registered.get(), 0);
    assert_eq!(fake.codex_registered.get(), 0);
    assert_eq!(fake.doctor_runs.get(), 0);
}

// ----- fresh installs -----

#[test]
fn fresh_kiconnect_only() {
    let fake = Fake::new();
    let (outcome, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0]),
            A::Secret(KIC_KEY),
            A::Yes, /* check online */
        ],
    );
    assert_eq!(outcome, Outcome::Done);
    let b = fake.resolved();
    assert_eq!(b.names(), ["kiconnect"]);
    assert_eq!(b.default_backend().name, "kiconnect");
    assert!(fake.written().starts_with(&templates::header()));
    assert_eq!(
        fake.secret(&SecretId::Backend("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY)
    );
    // The key went to GET {base_url}/models as bearer token.
    assert_eq!(
        *fake.probes.borrow(),
        [(
            "https://chat.kiconnect.nrw/api/v1/models".to_owned(),
            Some(KIC_KEY.to_owned())
        )]
    );
    assert_eq!(fake.registered.get(), 1);
    assert_eq!(fake.doctor_runs.get(), 1);
    assert!(fake.runs.borrow().is_empty());
    assert!(fake.cfg().parent().unwrap().join("logs").is_dir());
    for needle in [
        "Claude Code: ",
        "2.1.296",
        "Node.js: ",
        "OpenCode: ",
        "KI:connect.nrw (Hochschul-Kontingent, kostenlos) | Claude-Zweitkonto | Gemini | OpenAI Codex (ChatGPT-Abo oder API-Key) | Groq (kostenlos, mit Limits) default [0]",
        "Codex: nicht gefunden (nur für OpenAI Codex nötig)",
        "FAKE DOCTOR REPORT",
        "Developer: Reload Window",
        "/mcp",
        "Lass kiconnect die README zusammenfassen",
        "verlängert sich bei jeder",
        "agent-sidekick setup --backend kiconnect",
        "Schlüssel geprüft: HTTP 200",
    ] {
        assert!(out.contains(needle), "missing {needle:?}:\n{out}");
    }
    assert!(fake.backups().is_empty());
}

#[test]
fn fresh_all_three_with_gemini_as_default() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0, 1, 2]),
            A::One(2), // default: kiconnect | claude | gemini
            A::Secret(KIC_KEY),
            A::No, // no online check
            A::Secret(CLAUDE_TOKEN),
            A::Secret(GEMINI_KEY),
        ],
    );
    let b = fake.resolved();
    assert_eq!(b.names(), ["claude", "gemini", "kiconnect"]);
    assert_eq!(b.default_backend().name, "gemini");
    assert_eq!(fake.written().matches("default = true").count(), 1);
    assert_eq!(
        fake.secret(&SecretId::Backend("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY)
    );
    assert_eq!(
        fake.secret(&SecretId::legacy()).as_deref(),
        Some(CLAUDE_TOKEN)
    );
    assert_eq!(
        fake.secret(&SecretId::Command("gemini".into())).as_deref(),
        Some(GEMINI_KEY)
    );
    assert!(fake.probes.borrow().is_empty());
    // The Claude config dir of the second account was created (in the fake home).
    let BackendKind::ClaudeOauth { config_dir, .. } = &b.get("claude").unwrap().kind else {
        panic!()
    };
    assert!(config_dir.is_dir(), "{}", config_dir.display());
    // The Gemini command runs the located entry with the located node.
    let BackendKind::Command { command, .. } = &b.get("gemini").unwrap().kind else {
        panic!()
    };
    assert_eq!(Path::new(&command[0]), fake.node.as_deref().unwrap());
    assert_eq!(
        Path::new(&command[1]),
        fake.gemini.borrow().as_deref().unwrap()
    );
    assert!(out.contains("claude setup-token"), "{out}");
    assert!(out.contains("ZWEITE"), "{out}");
    assert!(out.contains(EXPECTED_PREFIX), "{out}");
    assert!(out.contains("https://aistudio.google.com/apikey"), "{out}");
    // The --skip-trust risk is named when Gemini is set up.
    assert!(out.contains(super::GEMINI_WARNING), "{out}");
    assert!(out.contains(".gemini/settings.json"), "{out}");
}

#[test]
fn fresh_claude_only_needs_no_node() {
    let mut fake = Fake::new();
    fake.node = None;
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![1]), A::Secret("not-a-claude-token-xyz")],
    );
    let b = fake.resolved();
    assert_eq!(b.names(), ["claude"]);
    assert!(b.default_backend().is_default);
    assert_eq!(
        fake.secret(&SecretId::legacy()).as_deref(),
        Some("not-a-claude-token-xyz")
    );
    assert!(
        out.contains("Warnung: Der Token beginnt nicht mit sk-ant-oat01-"),
        "{out}"
    );
    assert!(
        out.contains("Lass claude die README zusammenfassen"),
        "{out}"
    );
    assert!(!out.contains("verlängert sich bei jeder"), "{out}");
    assert!(!out.contains("Achtung, Gemini"), "{out}");
}

#[test]
fn fresh_claude_and_gemini_default_suggestion_without_kiconnect() {
    let fake = Fake::new();
    run_ok(
        &fake,
        vec![
            A::Many(vec![1, 2]),
            A::One(0), // claude | gemini
            A::Secret(CLAUDE_TOKEN),
            A::Secret(GEMINI_KEY),
        ],
    );
    let b = fake.resolved();
    assert_eq!(b.names(), ["claude", "gemini"]);
    assert_eq!(b.default_backend().name, "claude");
}

#[test]
fn fresh_kiconnect_and_claude() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0, 1]),
            A::One(0),
            A::Secret(KIC_KEY),
            A::Yes,
            A::Secret(CLAUDE_TOKEN),
        ],
    );
    assert_eq!(fake.resolved().default_backend().name, "kiconnect");
    assert!(out.contains("kiconnect (KI:connect.nrw"), "{out}");
}

#[test]
fn fresh_groq_only() {
    let fake = Fake::new();
    let (outcome, out) = run_ok(&fake, vec![A::Many(vec![4]), A::Secret(GROQ_KEY), A::Yes]);
    assert_eq!(outcome, Outcome::Done);
    let b = fake.resolved();
    assert_eq!(b.names(), ["groq"]);
    assert_eq!(b.default_backend().name, "groq");
    assert!(fake.written().contains("[backends.groq]\ndefault = true\n"));
    let id = SecretId::Backend("groq".into());
    assert_eq!(id.keyring_user(), "backend-groq");
    assert_eq!(fake.secret(&id).as_deref(), Some(GROQ_KEY));
    assert_eq!(fake.secrets.borrow().len(), 1);
    assert_eq!(
        *fake.probes.borrow(),
        [(
            "https://api.groq.com/openai/v1/models".to_owned(),
            Some(GROQ_KEY.to_owned())
        )]
    );
    assert!(fake.runs.borrow().is_empty());
    for needle in [
        "— Groq (kostenlos, mit Limits) (Backend `groq`) —",
        "https://console.groq.com/keys",
        "ohne Kreditkarte",
        "8000",
        "? Groq-API-Schlüssel einfügen (Eingabe unsichtbar): ",
        "Schlüssel geprüft: HTTP 200",
        "Provider-Schlüssel von Backend `groq` gespeichert",
        "Lass groq die README zusammenfassen",
    ] {
        assert!(out.contains(needle), "missing {needle:?}:\n{out}");
    }
    assert!(!out.contains("verlängert sich bei jeder"), "{out}");
}

#[test]
fn fresh_groq_and_kiconnect_suggests_kiconnect_as_default() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![4, 0]),
            A::One(1),
            A::Secret(GROQ_KEY),
            A::Yes,
            A::Secret(KIC_KEY),
            A::Yes,
        ],
    );
    assert!(
        out.contains(
            "groq (Groq (kostenlos, mit Limits)) | kiconnect (KI:connect.nrw \
             (Hochschul-Kontingent, kostenlos)) default 1"
        ),
        "{out}"
    );
    let b = fake.resolved();
    assert_eq!(b.names(), ["groq", "kiconnect"]);
    assert_eq!(b.default_backend().name, "kiconnect");
    assert_eq!(fake.written().matches("default = true").count(), 1);
    assert_eq!(
        fake.secret(&SecretId::Backend("groq".into())).as_deref(),
        Some(GROQ_KEY)
    );
    assert_eq!(
        fake.secret(&SecretId::Backend("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY)
    );
    assert_eq!(
        *fake.probes.borrow(),
        [
            (
                "https://api.groq.com/openai/v1/models".to_owned(),
                Some(GROQ_KEY.to_owned())
            ),
            (
                "https://chat.kiconnect.nrw/api/v1/models".to_owned(),
                Some(KIC_KEY.to_owned())
            ),
        ]
    );
    assert!(
        out.contains("Lass kiconnect die README zusammenfassen"),
        "{out}"
    );
}

#[test]
fn nothing_selected_is_asked_again_then_aborts() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![]), A::Many(vec![1]), A::Secret(CLAUDE_TOKEN)],
    );
    assert!(out.contains("Bitte mindestens ein Profil wählen."), "{out}");
    assert!(
        out.contains("OAuth-Token von Backend `claude` gespeichert im Fake-Schlüsselbund."),
        "{out}"
    );
    assert_eq!(fake.resolved().names(), ["claude"]);

    let fake = Fake::new();
    let (err, _) = run_err(
        &fake,
        vec![A::Many(vec![]), A::Many(vec![]), A::Many(vec![])],
    );
    assert!(err.contains("Kein Profil gewählt"), "{err}");
    assert!(err.contains("agent-sidekick setup"), "{err}");
    assert_nothing_written(&fake);
}

/// Neither Claude Code nor Codex: abort before anything is written, naming both installs.
#[test]
fn claude_code_and_codex_missing_aborts() {
    for os in [Os::Windows, Os::Unix] {
        let mut fake = Fake::new();
        fake.os = os;
        fake.claude = None;
        let (err, out) = run_err(&fake, vec![]);
        assert!(
            err.contains("Weder Claude Code noch die Codex-CLI"),
            "{err}"
        );
        assert!(
            err.contains("VS-Code-Erweiterung allein reicht nicht"),
            "{err}"
        );
        assert!(err.contains(super::claude_install_command(os)), "{err}");
        assert!(
            err.contains(&format!("npm install -g {}", npm::CODEX_PACKAGE)),
            "{err}"
        );
        assert!(err.contains("Node.js"), "{err}");
        assert!(err.contains("neues Terminal"), "{err}");
        assert!(err.contains("VS Code komplett neu"), "{err}");
        assert!(err.contains("setup` erneut"), "{err}");
        assert!(err.contains("Es wurde nichts geändert"), "{err}");
        assert!(out.contains("Claude Code: nicht gefunden"), "{out}");
        assert!(out.contains("Codex: nicht gefunden"), "{out}");
        assert_nothing_written(&fake);
        assert!(fake.runs.borrow().is_empty());
    }
    assert_eq!(npm::CODEX_PACKAGE, "@openai/codex");
    assert_eq!(
        super::claude_install_command(Os::Windows),
        "irm https://claude.ai/install.ps1 | iex"
    );
    assert_eq!(
        super::claude_install_command(Os::Unix),
        "curl -fsSL https://claude.ai/install.sh | bash"
    );
    // The same commands as the `claude` harness's install hint (checked against the docs).
    let hint = crate::harness::HarnessKind::Claude
        .adapter()
        .install_hint()
        .unwrap();
    for os in [Os::Windows, Os::Unix] {
        assert!(hint.contains(super::claude_install_command(os)), "{hint}");
    }
}

/// Codex installed, Claude Code not: the wizard goes on, offers no second Claude account,
/// registers only in Codex (without asking) and ends with the Codex next steps.
#[test]
fn claude_missing_with_codex_registers_only_in_codex() {
    let mut fake = Fake::new();
    fake.claude = None;
    *fake.codex.borrow_mut() = Some(fake.dir.path().join("npm").join("codex.exe"));
    let (outcome, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0]), // KI:connect.nrw
            A::Secret(KIC_KEY),
            A::Yes, // check online
        ],
    );
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(fake.resolved().names(), ["kiconnect"]);
    assert_eq!(fake.registered.get(), 0, "no Claude Code registration");
    assert_eq!(fake.codex_registered.get(), 1);
    assert_eq!(fake.doctor_runs.get(), 1);
    assert!(out.contains(super::CODEX_ONLY_NOTE), "{out}");
    assert!(out.contains("setup --register-only"), "{out}");
    assert!(out.contains("Claude Code: nicht gefunden"), "{out}");
    assert!(out.contains("Codex: "), "{out}");
    assert!(!out.contains("Claude-Zweitkonto"), "{out}");
    assert!(
        out.contains(
            "KI:connect.nrw (Hochschul-Kontingent, kostenlos) | Gemini | OpenAI Codex \
             (ChatGPT-Abo oder API-Key) | Groq (kostenlos, mit Limits) default [0]"
        ),
        "{out}"
    );
    assert!(!out.contains("Auch in Codex anmelden?"), "{out}");
    assert!(
        !out.contains("Developer: Reload Window“), dann in Claude Code"),
        "{out}"
    );
    assert!(out.contains("Eine neue Codex-Sitzung starten"), "{out}");
}

/// Without Claude Code the profile indices map onto the remaining profiles (index 2 is
/// OpenAI Codex), and an implicit `sidekick` of an existing config is not carried over.
#[test]
fn claude_missing_merge_into_implicit_config_drops_sidekick() {
    let mut fake = Fake::new();
    fake.claude = None;
    *fake.codex.borrow_mut() = Some(fake.dir.path().join("npm").join("codex.exe"));
    fake.seed_config(
        "max_concurrent = 2
",
    );
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![2]), // OpenAI Codex
            A::One(0),        // merge
            A::One(0),        // ChatGPT plan
            A::Yes,           // "Anmeldung erledigt?"
        ],
    );
    assert_eq!(fake.resolved().names(), ["codex"]);
    assert!(out.contains("wird deshalb nicht"), "{out}");
    assert_eq!(fake.registered.get(), 0);
    assert_eq!(fake.codex_registered.get(), 1);
}

// ----- Node.js and installs -----

#[test]
fn node_missing_aborts_with_instructions_and_writes_nothing() {
    for os in [Os::Windows, Os::Unix] {
        let mut fake = Fake::new();
        fake.os = os;
        fake.node = None;
        *fake.opencode.borrow_mut() = None;
        let (err, out) = run_err(&fake, vec![A::Many(vec![0])]);
        assert!(err.contains("Node.js fehlt"), "{err}");
        assert!(err.contains("`agent-sidekick setup` erneut"), "{err}");
        // A new VS Code terminal keeps VS Code's old PATH.
        assert!(err.contains("VS Code komplett neu"), "{err}");
        assert!(out.contains("VS Code komplett neu"), "{out}");
        assert!(out.contains("https://nodejs.org/en/download"), "{out}");
        match os {
            Os::Windows => assert!(out.contains("winget install OpenJS.NodeJS.LTS"), "{out}"),
            Os::Unix => {
                assert!(out.contains("nvm install --lts"), "{out}");
                assert!(out.contains("apt install nodejs"), "{out}");
            }
        }
        assert_nothing_written(&fake);
        assert!(fake.runs.borrow().is_empty());
    }
    // Gemini needs node even when OpenCode is installed.
    let mut fake = Fake::new();
    fake.node = None;
    let (err, _) = run_err(&fake, vec![A::Many(vec![0, 2])]);
    assert!(err.contains("Gemini CLI"), "{err}");
    assert_nothing_written(&fake);
}

#[test]
fn opencode_missing_is_installed_with_the_exact_argv() {
    let fake = Fake::new();
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0]),
            A::Yes, /* install */
            A::Secret(KIC_KEY),
            A::Yes,
        ],
    );
    let runs = fake.runs.borrow();
    assert_eq!(runs.len(), 1);
    let npm = fake.npm.as_ref().unwrap();
    assert_eq!(runs[0].program, npm.program);
    let mut expected = npm.args.clone();
    expected.extend(["install", "-g", "opencode-ai"].map(Into::into));
    assert_eq!(runs[0].args, expected);
    assert!(out.contains("npm install -g opencode-ai"), "{out}");
    assert!(out.contains("OpenCode installiert"), "{out}");
    assert!(fake.cfg().exists());
}

#[test]
fn groq_needs_node_and_opencode() {
    let mut fake = Fake::new();
    fake.node = None;
    *fake.opencode.borrow_mut() = None;
    let (err, _) = run_err(&fake, vec![A::Many(vec![4])]);
    assert!(err.contains("Node.js fehlt"), "{err}");
    assert!(err.contains("die Installation von OpenCode"), "{err}");
    assert_nothing_written(&fake);

    let fake = Fake::new();
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![4]), A::No, A::Secret(GROQ_KEY), A::No],
    );
    assert!(
        out.contains("Hinweis: Groq braucht OpenCode. Installiere es später mit `npm install -g opencode-ai`"),
        "{out}"
    );
    assert_eq!(fake.resolved().names(), ["groq"]);
}

#[test]
fn opencode_install_declined_gives_a_hint_and_continues() {
    let fake = Fake::new();
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![0]), A::No, A::Secret(KIC_KEY), A::Yes],
    );
    assert!(fake.runs.borrow().is_empty());
    assert!(
        out.contains("Installiere es später mit `npm install -g opencode-ai`"),
        "{out}"
    );
    assert_eq!(fake.resolved().names(), ["kiconnect"]);
}

#[test]
fn failed_npm_install_aborts_before_writing() {
    let mut fake = Fake::new();
    fake.install_ok = false;
    fake.os = Os::Unix;
    *fake.opencode.borrow_mut() = None;
    let (err, _) = run_err(&fake, vec![A::Many(vec![0]), A::Yes]);
    assert!(
        err.contains("npm install -g opencode-ai` ist fehlgeschlagen"),
        "{err}"
    );
    assert!(err.contains("EACCES"), "{err}");
    assert_eq!(fake.runs.borrow().len(), 1);
    assert_nothing_written(&fake);
}

#[test]
fn no_shell_free_npm_prints_the_manual_command() {
    let mut fake = Fake::new();
    fake.npm = None;
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(&fake, vec![A::Many(vec![0]), A::Secret(KIC_KEY), A::No]);
    assert!(
        out.contains("npm wurde neben Node.js nicht gefunden"),
        "{out}"
    );
    assert!(fake.runs.borrow().is_empty());
}

#[test]
fn gemini_missing_is_installed() {
    let fake = Fake::new();
    *fake.gemini.borrow_mut() = None;
    run_ok(&fake, vec![A::Many(vec![2]), A::Yes, A::Secret(GEMINI_KEY)]);
    let runs = fake.runs.borrow();
    assert_eq!(runs.len(), 1);
    let tail: Vec<String> = runs[0].args[1..]
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(tail, ["install", "-g", "@google/gemini-cli"]);
    let b = fake.resolved();
    let BackendKind::Command { command, .. } = &b.get("gemini").unwrap().kind else {
        panic!()
    };
    assert!(command[1].ends_with("gemini.js"), "{command:?}");
}

#[test]
fn gemini_declined_drops_the_profile() {
    let fake = Fake::new();
    *fake.gemini.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![1, 2]), A::No, A::Secret(CLAUDE_TOKEN)],
    );
    assert!(out.contains("Gemini wird übersprungen"), "{out}");
    assert_eq!(fake.resolved().names(), ["claude"]);

    // Gemini alone and declined: nothing left.
    let fake = Fake::new();
    *fake.gemini.borrow_mut() = None;
    let (err, _) = run_err(&fake, vec![A::Many(vec![2]), A::No]);
    assert!(err.contains("Kein Profil übrig"), "{err}");
    assert_nothing_written(&fake);
}

#[test]
fn gemini_without_npm_is_skipped_with_one_hint() {
    let mut fake = Fake::new();
    fake.npm = None;
    *fake.gemini.borrow_mut() = None;
    let (_, out) = run_ok(&fake, vec![A::Many(vec![1, 2]), A::Secret(CLAUDE_TOKEN)]);
    assert!(
        out.contains("npm wurde neben Node.js nicht gefunden"),
        "{out}"
    );
    assert!(out.contains("Gemini wird übersprungen"), "{out}");
    // The manual command is named once, and the wizard does not tell the user to start
    // over while it keeps going.
    assert_eq!(
        out.matches("npm install -g @google/gemini-cli").count(),
        1,
        "{out}"
    );
    assert!(!out.contains(RESUME), "{out}");
    assert_eq!(fake.resolved().names(), ["claude"]);
}

// ----- existing config -----

const EXAMPLE: &str = include_str!("../../../examples/config.toml");

#[test]
fn existing_config_cancel_changes_nothing() {
    let fake = Fake::new();
    fake.seed_config(EXAMPLE);
    let (outcome, out) = run_ok(&fake, vec![A::Many(vec![0]), A::One(2)]);
    assert_eq!(outcome, Outcome::Cancelled);
    assert_eq!(fake.written(), EXAMPLE);
    assert!(fake.backups().is_empty());
    assert!(fake.secrets.borrow().is_empty());
    assert_eq!(fake.registered.get(), 0);
    assert!(out.contains("nichts geändert"), "{out}");
}

#[test]
fn existing_config_backup_and_rewrite() {
    let fake = Fake::new();
    fake.seed_config(EXAMPLE);
    run_ok(
        &fake,
        vec![A::Many(vec![0]), A::One(1), A::Secret(KIC_KEY), A::Yes],
    );
    let backups = fake.backups();
    assert_eq!(backups.len(), 1);
    assert!(backups[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with("config.toml.bak-20261021-120000"));
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), EXAMPLE);
    assert!(fake.written().starts_with(&templates::header()));
    assert_eq!(fake.resolved().names(), ["kiconnect"]);

    // A second backup in the same second does not overwrite the first.
    // The key is stored now: "replace?" no skips the key prompt.
    run_ok(&fake, vec![A::Many(vec![0]), A::One(1), A::No]);
    assert_eq!(fake.backups().len(), 2);
}

#[test]
fn merge_into_the_example_keeps_existing_backends_byte_identical() {
    let fake = Fake::new();
    fake.seed_config(EXAMPLE);
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0, 2]),
            A::One(0),          // merge
            A::Secret(KIC_KEY), // the existing kiconnect (aider, command) has no key yet
            A::Secret(GEMINI_KEY),
        ],
    );
    let written = fake.written();
    assert!(written.starts_with(EXAMPLE), "existing text changed");
    assert!(!written[EXAMPLE.len()..].contains("[backends.kiconnect]"));
    assert!(written[EXAMPLE.len()..].contains("[backends.gemini]"));
    assert!(!written[EXAMPLE.len()..].contains("default = true"));
    let b = fake.resolved();
    assert_eq!(b.default_backend().name, "claude");
    assert!(matches!(
        b.get("kiconnect").unwrap().kind,
        BackendKind::Command { .. }
    ));
    // The key goes where the surviving backend reads it (command secret), not the
    // template's opencode id; a command backend is not probed.
    assert_eq!(
        fake.secret(&SecretId::Command("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY)
    );
    assert_eq!(fake.secret(&SecretId::Backend("kiconnect".into())), None);
    assert!(fake.probes.borrow().is_empty());
    assert!(
        out.contains("Backend `kiconnect` steht schon in der config.toml"),
        "{out}"
    );
    assert!(out.contains("Standard-Backend bleibt `claude`"), "{out}");
    assert_eq!(fake.backups().len(), 1);
}

/// A config from before v0.6 names the Claude backend `sidekick`: the Claude profile counts
/// as present (no second Claude section) and its token goes to the shared legacy secret.
#[test]
fn merge_keeps_an_old_sidekick_backend() {
    let fake = Fake::new();
    let old = EXAMPLE.replace("[backends.claude]", "[backends.sidekick]");
    fake.seed_config(&old);
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![1]), A::One(0), A::Secret(CLAUDE_TOKEN)],
    );
    assert_eq!(fake.written(), old);
    assert!(
        out.contains("Backend `sidekick` steht schon in der config.toml"),
        "{out}"
    );
    let id = SecretId::Oauth("sidekick".into());
    assert!(id.is_legacy());
    assert_eq!(fake.secret(&id).as_deref(), Some(CLAUDE_TOKEN));
}

#[test]
fn merge_with_nothing_missing_leaves_the_file_alone() {
    let fake = Fake::new();
    fake.seed_config(EXAMPLE);
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![1]), A::One(0), A::Secret(CLAUDE_TOKEN)],
    );
    assert_eq!(fake.written(), EXAMPLE);
    assert!(fake.backups().is_empty());
    assert!(out.contains("config.toml bleibt unverändert"), "{out}");
    assert_eq!(
        fake.secret(&SecretId::legacy()).as_deref(),
        Some(CLAUDE_TOKEN)
    );
    assert_eq!(fake.registered.get(), 1);
}

#[test]
fn merge_into_an_implicit_config_keeps_claude() {
    let fake = Fake::new();
    let old = "max_concurrent = 3\n# my comment\ndefault_timeout_s = 900";
    fake.seed_config(old);
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0]),
            A::One(0), // merge
            A::One(1), // default: kiconnect | claude -> claude
            A::Secret(KIC_KEY),
            A::Yes,
        ],
    );
    let written = fake.written();
    assert!(written.starts_with(old));
    let b = fake.resolved();
    assert_eq!(b.names(), ["claude", "kiconnect"]);
    assert_eq!(b.default_backend().name, "claude");
    assert!(out.contains("eingebaute Backend `claude`"), "{out}");
    // No token prompt for the carried-over implicit backend.
    assert_eq!(fake.secret(&SecretId::legacy()), None);
}

#[test]
fn unreadable_config_offers_only_rewrite_or_cancel() {
    let fake = Fake::new();
    fake.seed_config("this is [not toml");
    let (outcome, out) = run_ok(&fake, vec![A::Many(vec![1]), A::One(1)]);
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(
        out.contains("daher ist Zusammenführen nicht möglich"),
        "{out}"
    );
    assert!(
        !out.contains("? Wie soll es weitergehen? Zusammenführen"),
        "{out}"
    );
    assert_eq!(fake.written(), "this is [not toml");

    run_ok(
        &fake,
        vec![A::Many(vec![1]), A::One(0), A::Secret(CLAUDE_TOKEN)],
    );
    assert_eq!(fake.resolved().names(), ["claude"]);
    assert_eq!(fake.backups().len(), 1);
}

#[test]
fn merge_into_an_invalid_backend_config_names_the_way_out() {
    let fake = Fake::new();
    // Parses, but does not resolve (two defaults).
    let old = "[backends.a]\ntype = \"command\"\ndescription = \"a\"\ndefault = true\ncommand = [\"x\", \"{prompt}\"]\n\
               [backends.b]\ntype = \"command\"\ndescription = \"b\"\ndefault = true\ncommand = [\"x\", \"{prompt}\"]\n";
    fake.seed_config(old);
    let (err, _) = run_err(&fake, vec![A::Many(vec![1]), A::One(0)]);
    assert!(err.contains("Sichern und neu schreiben"), "{err}");
    assert_eq!(fake.written(), old);
    assert!(fake.secrets.borrow().is_empty());
}

// ----- secrets -----

#[test]
fn stored_secret_is_kept_unless_replaced() {
    let fake = Fake::new();
    fake.secrets
        .borrow_mut()
        .insert(SecretId::legacy(), "old-token-value-123".into());
    run_ok(&fake, vec![A::Many(vec![1]), A::No]);
    assert_eq!(
        fake.secret(&SecretId::legacy()).as_deref(),
        Some("old-token-value-123")
    );
}

#[test]
fn empty_or_multiline_secret_is_asked_again() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![2]),
            A::Secret("   "),
            A::Secret("two\nlines-SECRET"),
            A::Secret(GEMINI_KEY),
        ],
    );
    assert!(out.contains("Die Eingabe war leer"), "{out}");
    assert!(out.contains("Steuerzeichen"), "{out}");
    assert_eq!(
        fake.secret(&SecretId::Command("gemini".into())).as_deref(),
        Some(GEMINI_KEY)
    );

    // Three bad inputs: nothing stored, the next step is named.
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![2]),
            A::Secret(""),
            A::Secret(""),
            A::Secret(""),
            A::Yes, // go on without the key
        ],
    );
    assert!(
        out.contains("`agent-sidekick setup --backend gemini`"),
        "{out}"
    );
    assert!(fake.secrets.borrow().is_empty());
    assert!(fake.cfg().exists());

    // Not going on without the key (or EOF at that question): abort, nothing written.
    let fake = Fake::new();
    let (err, _) = run_err(
        &fake,
        vec![
            A::Many(vec![2]),
            A::Secret(""),
            A::Secret(""),
            A::Secret(""),
            A::No,
        ],
    );
    assert!(err.contains("Es wurde nichts geändert"), "{err}");
    assert_nothing_written(&fake);
}

#[test]
fn short_secret_gets_the_redaction_warning() {
    let fake = Fake::new();
    let (_, out) = run_ok(&fake, vec![A::Many(vec![2]), A::Secret("k3y")]);
    assert!(
        out.contains(&format!("kürzer als {MIN_SECRET_LEN} Zeichen")),
        "{out}"
    );
    assert_eq!(
        fake.secret(&SecretId::Command("gemini".into())).as_deref(),
        Some("k3y")
    );

    // A normal key: no warning.
    let fake = Fake::new();
    let (_, out) = run_ok(&fake, vec![A::Many(vec![2]), A::Secret(GEMINI_KEY)]);
    assert!(!out.contains("kürzer als"), "{out}");
}

/// A local HTTP server answering one status per connection, in order.
fn status_server(codes: &'static [u16]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for &code in codes {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let mut head = Vec::new();
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let resp =
                format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    url
}

/// Sends every probe to the local server instead of chat.kiconnect.nrw.
fn local_probe(server: String) -> ProbeFn {
    Box::new(move |_url, bearer| {
        reach::probe_get(
            &format!("{server}/api/v1/models"),
            bearer,
            std::time::Duration::from_secs(10),
        )
    })
}

#[test]
fn rejected_key_from_a_local_server_is_reported_and_asked_again() {
    let mut fake = Fake::new();
    fake.probe = local_probe(status_server(&[401, 200]));
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0]),
            A::Secret(KIC_KEY),
            A::Yes, // check
            A::Yes, // new key
            A::Secret(KIC_KEY2),
            A::Yes, // check
        ],
    );
    assert!(
        out.contains(
            "Schlüssel abgelehnt (HTTP 401) – erstelle einen neuen auf https://chat.kiconnect.nrw"
        ),
        "{out}"
    );
    assert!(out.contains("Schlüssel geprüft: HTTP 200"), "{out}");
    assert_eq!(
        fake.secret(&SecretId::Backend("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY2)
    );
}

#[test]
fn rejected_key_declined_stores_nothing() {
    let mut fake = Fake::new();
    fake.probe = local_probe(status_server(&[403]));
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![0]), A::Secret(KIC_KEY), A::Yes, A::No],
    );
    assert!(out.contains("Schlüssel abgelehnt (HTTP 403)"), "{out}");
    assert!(
        out.contains("`agent-sidekick setup --backend kiconnect`"),
        "{out}"
    );
    assert!(fake.secrets.borrow().is_empty());
    // The rest of the setup still ran.
    assert!(fake.cfg().exists());
    assert_eq!(fake.doctor_runs.get(), 1);
}

#[test]
fn rejected_groq_key_names_the_groq_console() {
    let mut fake = Fake::new();
    fake.probe = local_probe(status_server(&[401]));
    let (_, out) = run_ok(
        &fake,
        vec![A::Many(vec![4]), A::Secret(GROQ_KEY), A::Yes, A::No],
    );
    assert!(
        out.contains(
            "Schlüssel abgelehnt (HTTP 401) – erstelle einen neuen auf https://console.groq.com/keys"
        ),
        "{out}"
    );
    assert!(
        out.contains("`agent-sidekick setup --backend groq`"),
        "{out}"
    );
    assert!(fake.secrets.borrow().is_empty());
    assert!(fake.cfg().exists());
}

#[test]
fn unreachable_provider_keeps_the_key() {
    let mut fake = Fake::new();
    fake.probe = Box::new(|_, _| ProbeResponse::Failed("connection refused".into()));
    let (_, out) = run_ok(&fake, vec![A::Many(vec![0]), A::Secret(KIC_KEY), A::Yes]);
    assert!(
        out.contains("Prüfung nicht möglich (connection refused)"),
        "{out}"
    );
    assert!(
        out.contains("agent-sidekick backends test kiconnect"),
        "{out}"
    );
    assert_eq!(
        fake.secret(&SecretId::Backend("kiconnect".into()))
            .as_deref(),
        Some(KIC_KEY)
    );
}

// ----- registration and doctor -----

#[test]
fn failed_registration_names_the_next_step_and_doctor_still_runs() {
    let mut fake = Fake::new();
    fake.register_ok = false;
    let (outcome, out) = run_ok(&fake, vec![A::Many(vec![1]), A::Secret(CLAUDE_TOKEN)]);
    assert_eq!(outcome, Outcome::Done);
    assert!(
        out.contains("agent-sidekick setup --register-only"),
        "{out}"
    );
    assert_eq!(fake.doctor_runs.get(), 1);
    let doctor_at = out.find("FAKE DOCTOR REPORT").unwrap();
    let register_at = out.find("Anmeldung bei Claude Code").unwrap();
    assert!(register_at < doctor_at);
}

#[test]
fn backup_names_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    let first = backup_path(&cfg, "T");
    assert_eq!(first, dir.path().join("config.toml.bak-T"));
    std::fs::write(&first, "").unwrap();
    assert_eq!(
        backup_path(&cfg, "T"),
        dir.path().join("config.toml.bak-T-2")
    );
}

#[test]
fn merge_without_final_line_break_and_nothing_to_add_is_not_rewritten() {
    let fake = Fake::new();
    let old = EXAMPLE.trim_end();
    fake.seed_config(old);
    run_ok(
        &fake,
        vec![A::Many(vec![1]), A::One(0), A::Secret(CLAUDE_TOKEN)],
    );
    assert_eq!(fake.written(), old);
    assert!(fake.backups().is_empty());
}

const OPENAI_KEY: &str = "sk-proj-SECRET-openai-key-0123456789";

/// Codex installed, ChatGPT plan: no key, the login command for the backend's own
/// CODEX_HOME, then the registration in Codex as well.
#[test]
fn fresh_codex_with_chatgpt_login_and_codex_registration() {
    let fake = Fake::new();
    *fake.codex.borrow_mut() = Some(fake.dir.path().join("npm").join("codex.exe"));
    let (outcome, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![3]),
            A::One(0), // ChatGPT plan
            A::Yes,    // "Anmeldung erledigt?" (not really: no auth.json)
            A::Yes,    // also register in Codex
        ],
    );
    assert_eq!(outcome, Outcome::Done);
    let b = fake.resolved();
    assert_eq!(b.names(), ["codex"]);
    let codex = b.get("codex").unwrap();
    let BackendKind::Codex { auth, base_dir, .. } = &codex.kind else {
        panic!("{codex:?}")
    };
    assert_eq!(*auth, CodexAuth::Chatgpt);
    assert_eq!(codex.max_concurrent, Some(1));
    // Its CODEX_HOME exists with the generated config before the user logs in.
    assert!(base_dir.join("config.toml").is_file());
    assert!(
        fake.secrets.borrow().is_empty(),
        "no key for a ChatGPT login"
    );
    assert_eq!(fake.registered.get(), 1);
    assert_eq!(fake.codex_registered.get(), 1);
    for needle in [
        "Codex: ",
        "codex-cli 0.163.0",
        "OpenAI Codex (ChatGPT-Abo oder API-Key)",
        "Wie soll OpenAI Codex abgerechnet werden?",
        "codex login",
        &base_dir.display().to_string(),
        "Noch keine Anmeldung gefunden",
        "agent-sidekick backends test codex",
        "Auch in Codex anmelden?",
        "In Codex: eine neue Sitzung starten",
    ] {
        assert!(out.contains(needle), "missing {needle:?}:\n{out}");
    }

    // Logged in by now: a second run finds the login and asks nothing about it.
    std::fs::write(base_dir.join("auth.json"), "{}").unwrap();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![3]),
            A::One(0), // merge: the existing backend stays
            A::No,     // not in Codex this time
        ],
    );
    assert!(
        out.contains("schon eine ChatGPT-Anmeldung vorhanden"),
        "{out}"
    );
    assert_eq!(fake.codex_registered.get(), 1);
}

/// Codex missing: installed through npm; API key: stored as backend-codex after the online
/// check against OpenAI's model list.
#[test]
fn fresh_codex_with_api_key_installs_codex() {
    let fake = Fake::new();
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![3]),
            A::Yes,    // install Codex via npm
            A::One(1), // API key
            A::Secret(OPENAI_KEY),
            A::Yes, // check online
            A::Yes, // register in Codex (installed now)
        ],
    );
    let runs = fake.runs.borrow();
    assert_eq!(runs.len(), 1);
    assert!(runs[0]
        .args
        .iter()
        .any(|a| a.to_string_lossy() == npm::CODEX_PACKAGE));
    let b = fake.resolved();
    let codex = b.get("codex").unwrap();
    assert!(matches!(
        codex.kind,
        BackendKind::Codex {
            auth: CodexAuth::ApiKey,
            ..
        }
    ));
    assert_eq!(
        fake.secret(&SecretId::Backend("codex".into())).as_deref(),
        Some(OPENAI_KEY)
    );
    assert_eq!(
        *fake.probes.borrow(),
        [(
            "https://api.openai.com/v1/models".to_owned(),
            Some(OPENAI_KEY.to_owned())
        )]
    );
    assert_eq!(fake.codex_registered.get(), 1);
    assert!(out.contains("platform.openai.com/api-keys"), "{out}");
    assert!(!out.contains("codex login"), "{out}");
}

/// Prints a typical run (for documentation): `cargo test typical_run -- --ignored --nocapture`.
#[test]
#[ignore]
fn typical_run_transcript() {
    let fake = Fake::new();
    *fake.opencode.borrow_mut() = None;
    let (_, out) = run_ok(
        &fake,
        vec![
            A::Many(vec![0, 1]),
            A::Yes,
            A::One(0),
            A::Secret(KIC_KEY),
            A::Yes,
            A::Secret(CLAUDE_TOKEN),
        ],
    );
    println!("{out}");
}
