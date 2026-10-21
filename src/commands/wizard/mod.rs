//! `agent-sidekick setup` without flags in an interactive terminal: a guided setup.
//!
//! Steps: introduction, prerequisites (Claude Code, Node.js, OpenCode), profiles
//! (KI:connect.nrw, second Claude account, Gemini), installing a missing OpenCode or
//! Gemini CLI through npm (after confirmation), the default backend, writing config.toml
//! from the embedded templates (fresh, backup + rewrite, or merge), the secrets, the MCP
//! registration and a final `doctor` run.
//!
//! The wizard talks German (target audience: universities in NRW, see NOTES.md); output
//! reused from other commands (registration lines, the doctor report) stays English.
//! All side effects go through [`system::System`] and all dialogue through
//! [`io::Prompter`], so the whole flow is tested with scripted answers.

pub mod io;
pub mod npm;
pub mod system;
pub mod templates;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use crate::backends::reach::ProbeResponse;
use crate::backends::{Backend, BackendKind, Backends};
use crate::config::Config;
use crate::locate::Os;
use crate::redact::redact_all;
use crate::token::{SecretId, Token, EXPECTED_PREFIX};
use crate::APP_NAME;
use io::Prompter;
use system::{System, Tool};
use templates::GeminiPaths;

/// A profile the user can pick; each one becomes one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Kiconnect,
    Claude,
    Gemini,
}

impl Profile {
    /// In the order the wizard offers them.
    pub const ALL: [Profile; 3] = [Profile::Kiconnect, Profile::Claude, Profile::Gemini];

    pub fn backend_name(self) -> &'static str {
        match self {
            Profile::Kiconnect => "kiconnect",
            Profile::Claude => "sidekick",
            Profile::Gemini => "gemini",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Profile::Kiconnect => "KI:connect.nrw (Hochschul-Kontingent, kostenlos)",
            Profile::Claude => "Claude-Zweitkonto",
            Profile::Gemini => "Gemini",
        }
    }

    /// The secret of the backend the template creates.
    pub fn secret_id(self) -> SecretId {
        match self {
            Profile::Kiconnect => SecretId::Backend("kiconnect".into()),
            Profile::Claude => SecretId::legacy(),
            Profile::Gemini => SecretId::Command("gemini".into()),
        }
    }
}

/// How the wizard ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
}

/// The wizard on the real terminal and machine.
pub fn run_interactive() -> Result<ExitCode> {
    run(&mut io::Terminal, &system::RealSystem)?;
    Ok(ExitCode::SUCCESS)
}

const INTRO: &str = "Willkommen beim Einrichtungsassistenten von agent-sidekick.\n\
    agent-sidekick lässt Claude Code Aufgaben an einen zweiten Agenten abgeben, zum Beispiel \
    an KI:connect.nrw, ein zweites Claude-Konto oder Gemini.\n\
    Der Assistent prüft die Voraussetzungen, schreibt die Konfiguration, speichert deine \
    Schlüssel sicher im Schlüsselbund und meldet den Server bei Claude Code an.";

const RESUME: &str = "Führe danach `agent-sidekick setup` erneut aus.";

const DIR_NEXT_STEP: &str = "Ordner konnte nicht angelegt werden. Nächster Schritt: \
    Zugriffsrechte prüfen, dann `agent-sidekick setup` erneut ausführen";

/// The whole wizard. `Err`: aborted with a message that names the next step.
pub fn run(p: &mut dyn Prompter, s: &dyn System) -> Result<Outcome> {
    p.say(INTRO);

    let cfg_path = s.config_file().context(
        "Der Konfigurationsordner kann nicht bestimmt werden. Nächster Schritt: \
         AGENT_SIDEKICK_CONFIG_DIR auf einen Ordner setzen, dann `agent-sidekick setup` erneut \
         ausführen",
    )?;
    let existing = read_existing(&cfg_path)?;
    let claude_path = existing
        .as_ref()
        .and_then(|e| e.parsed.as_ref().ok())
        .and_then(|c| c.claude_path.clone());

    // 1. Prerequisites.
    p.say("\nVoraussetzungen:");
    let claude = match s.find_claude(claude_path.as_deref()) {
        Ok(path) => {
            p.say(&format!(
                "  Claude Code: {} ({})",
                path.display(),
                version_text(s, Tool::Claude, &path)
            ));
            path
        }
        Err(e) => {
            p.say(&format!(
                "  Claude Code: nicht gefunden ({})",
                redact_all(&e)
            ));
            bail!(
                "Claude Code wurde nicht gefunden. Nächster Schritt: Claude Code installieren \
                 (https://docs.claude.com/en/docs/claude-code/setup) oder den Pfad in \
                 AGENT_SIDEKICK_CLAUDE setzen. {RESUME}"
            );
        }
    };
    let node = s.find_node();
    match &node {
        Some(path) => p.say(&format!(
            "  Node.js: {} ({})",
            path.display(),
            version_text(s, Tool::Node, path)
        )),
        None => p.say("  Node.js: nicht gefunden (nur für OpenCode und Gemini nötig)"),
    }
    let mut opencode = s.find_opencode();
    match &opencode {
        Some(path) => p.say(&format!(
            "  OpenCode: {} ({})",
            path.display(),
            version_text(s, Tool::OpenCode, path)
        )),
        None => p.say("  OpenCode: nicht gefunden (nur für KI:connect.nrw nötig)"),
    }

    // 2. Profiles.
    let mut profiles = choose_profiles(p)?;

    // 3. Tools for the profiles (nothing is written before this step succeeds).
    let needs_node = (profiles.contains(&Profile::Kiconnect) && opencode.is_none())
        || profiles.contains(&Profile::Gemini);
    let node = match node {
        Some(n) => Some(n),
        None if needs_node => {
            p.say(&node_instructions(s.os()));
            bail!(
                "Node.js fehlt (gebraucht für {}). Nächster Schritt: Node.js installieren, ein \
                 neues Terminal öffnen. {RESUME} Es wurde nichts geändert.",
                node_users(&profiles, opencode.is_none())
            );
        }
        None => None,
    };
    if profiles.contains(&Profile::Kiconnect) && opencode.is_none() {
        let node = node.as_deref().expect("checked above");
        if offer_install(p, s, node, npm::OPENCODE_PACKAGE, "OpenCode")? {
            opencode = s.find_opencode();
            match &opencode {
                Some(path) => p.say(&format!("OpenCode installiert: {}", path.display())),
                None => p.say(
                    "OpenCode wurde installiert, aber noch nicht gefunden. Nächster Schritt: \
                     ein neues Terminal öffnen und `agent-sidekick doctor` ausführen.",
                ),
            }
        } else {
            p.say(&format!(
                "Hinweis: KI:connect.nrw braucht OpenCode. Installiere es später mit \
                 `npm install -g {}` und prüfe dann mit `agent-sidekick doctor`.",
                npm::OPENCODE_PACKAGE
            ));
        }
    }
    let mut gemini_entry = None;
    if profiles.contains(&Profile::Gemini) {
        let node = node.as_deref().expect("checked above");
        let npm_base = s.npm(node);
        let mut entry = s.find_gemini(node, npm_base.as_ref());
        if entry.is_none() && offer_install(p, s, node, npm::GEMINI_PACKAGE, "Gemini CLI")? {
            entry = s.find_gemini(node, s.npm(node).as_ref());
            if entry.is_none() {
                bail!(
                    "Die Gemini CLI wurde installiert, aber nicht gefunden. Nächster Schritt: \
                     ein neues Terminal öffnen. {RESUME}"
                );
            }
        }
        match entry {
            Some(e) => {
                p.say(&format!("  Gemini CLI: {}", e.display()));
                gemini_entry = Some((node.to_path_buf(), e));
            }
            None => {
                profiles.retain(|&x| x != Profile::Gemini);
                if profiles.is_empty() {
                    bail!(
                        "Kein Profil übrig. Nächster Schritt: die Gemini CLI installieren \
                         (`npm install -g {}`) oder ein anderes Profil wählen. {RESUME}",
                        npm::GEMINI_PACKAGE
                    );
                }
                p.say(&format!(
                    "Gemini wird übersprungen; die übrigen Profile werden jetzt eingerichtet. \
                     Gemini später hinzufügen: `npm install -g {}`, dann `agent-sidekick setup` \
                     noch einmal ausführen.",
                    npm::GEMINI_PACKAGE
                ));
            }
        }
    }
    let gemini_paths = gemini_entry
        .as_ref()
        .map(|(node, entry)| GeminiPaths { node, entry });

    // 4. The config file.
    let mode = match choose_mode(p, existing, &cfg_path)? {
        Some(m) => m,
        None => {
            p.say("Abgebrochen; es wurde nichts geändert.");
            return Ok(Outcome::Cancelled);
        }
    };
    let (existing_backends, existing_default) = match &mode {
        Mode::Merge { cfg, .. } => {
            let map = cfg.backends.clone();
            let default = map.as_ref().and_then(|m| {
                m.iter()
                    .find(|(_, b)| b.default == Some(true))
                    .map(|(n, _)| n.clone())
            });
            (map, default)
        }
        _ => (None, None),
    };
    let merging = matches!(mode, Mode::Merge { .. });

    // Backends to add: the chosen profiles that do not exist yet, plus (merging into a
    // config without [backends]) the implicit `sidekick`, which a [backends] section would
    // otherwise remove.
    let mut add: Vec<Profile> = Vec::new();
    for &prof in &profiles {
        let exists = existing_backends
            .as_ref()
            .is_some_and(|m| m.contains_key(prof.backend_name()));
        if exists {
            p.say(&format!(
                "Backend `{}` steht schon in der config.toml und bleibt unverändert.",
                prof.backend_name()
            ));
        } else {
            add.push(prof);
        }
    }
    if merging && existing_backends.is_none() && !add.contains(&Profile::Claude) && !add.is_empty()
    {
        p.say(
            "Die bisherige Konfiguration nutzt das eingebaute Backend `sidekick` (zweites \
             Claude-Konto). Es wird als eigener Abschnitt übernommen, damit es erhalten bleibt; \
             sein gespeicherter Token gilt weiter.",
        );
        add.push(Profile::Claude);
    }

    // 5. The default backend.
    let default = match (&existing_default, add.as_slice()) {
        (Some(d), _) => {
            p.say(&format!(
                "Standard-Backend bleibt `{d}` (bestehende Einstellung, wird nicht verändert)."
            ));
            None
        }
        (None, []) => None,
        (None, [only]) => Some(*only),
        (None, many) => {
            let options: Vec<String> = many
                .iter()
                .map(|x| format!("{} ({})", x.backend_name(), x.label()))
                .collect();
            let suggestion = many
                .iter()
                .position(|&x| x == Profile::Kiconnect)
                .unwrap_or(0);
            let i = p.choose_one(
                "\nWelches Backend soll Standard sein (wenn Claude keines ausdrücklich wählt)?",
                &options,
                suggestion,
            )?;
            Some(many[i])
        }
    };

    // 6. Build and validate the new config before anything is asked or written.
    let mut sections = Vec::new();
    for &prof in &add {
        sections.push(
            templates::section(prof, default == Some(prof), gemini_paths.as_ref())
                .map_err(anyhow::Error::msg)?,
        );
    }
    let text = match &mode {
        Mode::Fresh | Mode::Rewrite => templates::fresh_config(&sections),
        Mode::Merge { text, .. } => templates::merged_config(text, &sections),
    };
    let (_, backends) = templates::validate(&text, &s.resolve_defaults()).map_err(|e| {
        anyhow::anyhow!(
            "Die neue Konfiguration wäre ungültig:\n{}\nNächster Schritt: `agent-sidekick setup` \
             erneut starten und „Sichern und neu schreiben“ wählen, oder {} korrigieren. Es \
             wurde nichts geändert.",
            redact_all(&e),
            cfg_path.display()
        )
    })?;

    // 7. Secrets (asked before writing, so a cancelled prompt leaves nothing behind).
    let mut secrets: Vec<(String, SecretId, Token)> = Vec::new();
    for &prof in &profiles {
        let Some(backend) = backends.get(prof.backend_name()) else {
            continue;
        };
        if let Some((id, token)) = ask_secret(p, s, prof, backend)? {
            secrets.push((backend.name.clone(), id, token));
        }
    }

    // 8. Write.
    let write_needed = !(merging && sections.is_empty());
    if write_needed {
        write_config(p, s, &cfg_path, &mode, &text)?;
    } else {
        p.say(&format!(
            "\nconfig.toml bleibt unverändert: {}",
            cfg_path.display()
        ));
    }
    if let Some(dir) = cfg_path.parent() {
        s.create_dir(&dir.join("logs")).context(DIR_NEXT_STEP)?;
    }
    create_claude_dirs(s, &backends, &add).context(DIR_NEXT_STEP)?;
    for (name, id, token) in &secrets {
        let source = s.store_secret(id, token).map_err(|e| {
            anyhow::anyhow!(
                "{}: Speichern fehlgeschlagen: {}. Nächster Schritt: `agent-sidekick setup \
                 --backend {name}`",
                id,
                redact_all(&format!("{e:#}"))
            )
        })?;
        p.say(&format!("Gespeichert: {id} in {source}"));
    }
    drop(secrets);

    // 9. Registration with Claude Code.
    p.say("\nAnmeldung bei Claude Code:");
    if let Err(e) = s.register(&claude) {
        p.say(&format!(
            "Die Anmeldung bei Claude Code ist fehlgeschlagen: {}\nNächster Schritt: \
             `agent-sidekick setup --register-only` ausführen.",
            redact_all(&format!("{e:#}"))
        ));
    }

    // 10. Doctor.
    p.say("\nPrüfung (agent-sidekick doctor):");
    p.say(s.doctor().trim_end());

    // 11. Finish.
    let example = example_backend(&profiles);
    p.say(&format!(
        "\nFertig. Nächste Schritte:\n\
         1. VS Code neu laden (Befehlspalette: „Developer: Reload Window“), dann in Claude Code \
         /mcp prüfen: `{APP_NAME}` sollte als verbunden erscheinen.\n\
         2. Ausprobieren, z. B. mit dem Prompt: „Lass {example} die README zusammenfassen“."
    ));
    if profiles.contains(&Profile::Kiconnect) {
        p.say(
            "Hinweis: Ein KI:connect-Schlüssel läuft nach einem Monat ab. Einen neuen speicherst \
             du mit `agent-sidekick setup --backend kiconnect`.",
        );
    }
    Ok(Outcome::Done)
}

fn version_text(s: &dyn System, tool: Tool, path: &Path) -> String {
    match s.version(tool, path) {
        Ok(v) if !v.is_empty() => redact_all(&v),
        Ok(_) => "Version unbekannt".to_owned(),
        Err(e) => format!("Version nicht lesbar: {}", redact_all(&e)),
    }
}

fn node_users(profiles: &[Profile], opencode_missing: bool) -> String {
    let mut users = Vec::new();
    if opencode_missing && profiles.contains(&Profile::Kiconnect) {
        users.push("die Installation von OpenCode");
    }
    if profiles.contains(&Profile::Gemini) {
        users.push("die Gemini CLI");
    }
    users.join(" und ")
}

/// How to install Node.js on this platform.
pub fn node_instructions(os: Os) -> String {
    let how = match os {
        Os::Windows => {
            "  - Installer (LTS): https://nodejs.org/en/download\n\
             \x20 - oder im Terminal: winget install OpenJS.NodeJS.LTS"
        }
        Os::Unix => {
            "  - Download und Anleitungen: https://nodejs.org/en/download\n\
             \x20 - über die Paketverwaltung der Distribution (Node.js 20 oder neuer, mit npm), \
             z. B. sudo apt install nodejs npm\n\
             \x20 - oder mit nvm (https://github.com/nvm-sh/nvm): nvm install --lts"
        }
    };
    format!(
        "\nNode.js wird gebraucht, ist aber nicht installiert. So installierst du es:\n{how}\n\
         Öffne danach ein neues Terminal. {RESUME}"
    )
}

fn choose_profiles(p: &mut dyn Prompter) -> Result<Vec<Profile>> {
    let options: Vec<String> = Profile::ALL.iter().map(|x| x.label().to_owned()).collect();
    for _ in 0..3 {
        let picked = p.choose_many(
            "\nWelche Profile möchtest du einrichten? (mehrere möglich)",
            &options,
            &[0],
        )?;
        if !picked.is_empty() {
            return Ok(picked.into_iter().map(|i| Profile::ALL[i]).collect());
        }
        p.say("Bitte mindestens ein Profil wählen.");
    }
    bail!(
        "Kein Profil gewählt. Nächster Schritt: `agent-sidekick setup` erneut starten und \
         mindestens ein Profil wählen. Es wurde nichts geändert."
    )
}

/// Offer `npm install -g <package>`. `Ok(true)`: installed; `Ok(false)`: declined or no
/// npm that runs without a shell (a hint was printed). `Err`: npm failed.
fn offer_install(
    p: &mut dyn Prompter,
    s: &dyn System,
    node: &Path,
    package: &str,
    what: &str,
) -> Result<bool> {
    let Some(base) = s.npm(node) else {
        // The caller names the manual `npm install -g` command next; not repeated here.
        p.say(&format!(
            "{what} ist nicht installiert, und npm wurde neben Node.js nicht gefunden."
        ));
        return Ok(false);
    };
    let inv = npm::install(&base, package);
    p.say(&format!(
        "\n{what} ist nicht installiert. Installationsbefehl:\n  npm install -g {package}\n\
         (ausgeführt als: {})",
        inv.display()
    ));
    if !p.confirm(&format!("{what} jetzt installieren?"), true)? {
        return Ok(false);
    }
    let ok = s.run_visible(&inv).with_context(|| {
        format!(
            "npm konnte nicht gestartet werden. Nächster Schritt: `npm install -g {package}` \
             selbst im Terminal ausführen. {RESUME}"
        )
    })?;
    if !ok {
        let extra = match s.os() {
            Os::Unix => {
                " Bei „EACCES“/fehlenden Rechten: ein Benutzer-Präfix einrichten \
                 (npm config set prefix ~/.local) oder Node.js über nvm installieren."
            }
            Os::Windows => "",
        };
        bail!(
            "`npm install -g {package}` ist fehlgeschlagen (Ausgabe siehe oben). Nächster \
             Schritt: den Befehl selbst im Terminal ausführen.{extra} {RESUME}"
        );
    }
    Ok(true)
}

/// The existing config.toml, if any.
struct Existing {
    text: String,
    parsed: Result<Config, String>,
}

fn read_existing(path: &Path) -> Result<Option<Existing>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let parsed = toml::from_str::<Config>(&text).map_err(|e| e.to_string());
            Ok(Some(Existing { text, parsed }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| {
            format!(
                "{} kann nicht gelesen werden. Nächster Schritt: Zugriffsrechte prüfen, dann \
                 `agent-sidekick setup` erneut ausführen",
                path.display()
            )
        }),
    }
}

enum Mode {
    Fresh,
    Rewrite,
    Merge { text: String, cfg: Config },
}

/// `None`: cancelled.
fn choose_mode(
    p: &mut dyn Prompter,
    existing: Option<Existing>,
    path: &Path,
) -> Result<Option<Mode>> {
    let Some(existing) = existing else {
        return Ok(Some(Mode::Fresh));
    };
    p.say(&format!(
        "\nEs gibt schon eine Konfiguration: {}",
        path.display()
    ));
    let rewrite = "Sichern (config.toml.bak-<Zeitstempel>) und neu schreiben".to_owned();
    let cancel = "Abbrechen (nichts ändern)".to_owned();
    match existing.parsed {
        Ok(cfg) => {
            let options = vec![
                "Zusammenführen: nur fehlende Backends ergänzen, bestehende bleiben unverändert"
                    .to_owned(),
                rewrite,
                cancel,
            ];
            Ok(
                match p.choose_one("Wie soll es weitergehen?", &options, 0)? {
                    0 => Some(Mode::Merge {
                        text: existing.text,
                        cfg,
                    }),
                    1 => Some(Mode::Rewrite),
                    _ => None,
                },
            )
        }
        Err(e) => {
            p.say(&format!(
                "Sie ist nicht lesbar ({}), daher ist Zusammenführen nicht möglich.",
                redact_all(&e)
            ));
            Ok(
                match p.choose_one("Wie soll es weitergehen?", &[rewrite, cancel], 0)? {
                    0 => Some(Mode::Rewrite),
                    _ => None,
                },
            )
        }
    }
}

fn write_config(
    p: &mut dyn Prompter,
    s: &dyn System,
    path: &Path,
    mode: &Mode,
    text: &str,
) -> Result<()> {
    let next = "Nächster Schritt: Zugriffsrechte des Konfigurationsordners prüfen, dann \
                `agent-sidekick setup` erneut ausführen";
    if let Some(dir) = path.parent() {
        s.create_dir(dir).with_context(|| next.to_owned())?;
    }
    if matches!(mode, Mode::Rewrite | Mode::Merge { .. }) {
        let backup = backup_path(path, &s.timestamp());
        std::fs::copy(path, &backup).with_context(|| {
            format!("Sicherung nach {} fehlgeschlagen. {next}", backup.display())
        })?;
        p.say(&format!("\nSicherung: {}", backup.display()));
    }
    let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
        .with_context(|| format!("{} konnte nicht geschrieben werden. {next}", path.display()))?;
    p.say(&format!("Konfiguration geschrieben: {}", path.display()));
    Ok(())
}

/// `config.toml.bak-<timestamp>` next to `path` (with a counter if that exists already).
fn backup_path(path: &Path, timestamp: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_owned());
    let mut candidate = path.with_file_name(format!("{name}.bak-{timestamp}"));
    let mut n = 1;
    while candidate.exists() {
        n += 1;
        candidate = path.with_file_name(format!("{name}.bak-{timestamp}-{n}"));
    }
    candidate
}

/// The Claude config dirs of the added claude-oauth backends (as `setup` does).
fn create_claude_dirs(s: &dyn System, backends: &Backends, added: &[Profile]) -> Result<()> {
    for prof in added {
        if let Some(BackendKind::ClaudeOauth { config_dir }) =
            backends.get(prof.backend_name()).map(|b| &b.kind)
        {
            s.create_dir(config_dir)?;
        }
    }
    Ok(())
}

/// Ask for the secret of `backend` (created from `prof`, or an existing backend of that
/// name). `None`: nothing to store (no secret needed, kept, or skipped).
fn ask_secret(
    p: &mut dyn Prompter,
    s: &dyn System,
    prof: Profile,
    backend: &Backend,
) -> Result<Option<(SecretId, Token)>> {
    let name = &backend.name;
    let Some(id) = backend.secret_id() else {
        p.say(&format!(
            "Backend `{name}`: kein gespeicherter Schlüssel nötig (z. B. Umgebungsvariable)."
        ));
        return Ok(None);
    };
    p.say(&format!("\n— {} (Backend `{name}`) —", prof.label()));
    if let Some(source) = s.stored_secret(&id) {
        let replace = p.confirm(
            &format!("Für `{name}` ist schon ein Schlüssel gespeichert ({source}). Ersetzen?"),
            false,
        )?;
        if !replace {
            return Ok(None);
        }
    }
    p.say(&secret_instructions(prof, backend, s.os()));
    let prompt = match prof {
        Profile::Kiconnect => "KI:connect-API-Schlüssel einfügen (Eingabe unsichtbar): ",
        Profile::Claude => "OAuth-Token einfügen (Eingabe unsichtbar): ",
        Profile::Gemini => "Gemini-API-Schlüssel einfügen (Eingabe unsichtbar): ",
    };
    loop {
        let Some(token) = read_valid_secret(p, prompt)? else {
            p.say(&format!(
                "Nichts gespeichert. Nächster Schritt: später `agent-sidekick setup --backend \
                 {name}` ausführen."
            ));
            return Ok(None);
        };
        if prof == Profile::Claude && !token.expose().starts_with(EXPECTED_PREFIX) {
            p.say(&format!(
                "Warnung: Der Token beginnt nicht mit {EXPECTED_PREFIX}; vermutlich ist es kein \
                 Token aus `claude setup-token`. Er wird trotzdem gespeichert."
            ));
        }
        match verify_key(p, s, prof, backend, &token)? {
            Verified::Keep => return Ok(Some((id, token))),
            Verified::Retry => continue,
            Verified::Skip => {
                p.say(&format!(
                    "Schlüssel nicht gespeichert. Nächster Schritt: einen neuen Schlüssel \
                     erstellen und `agent-sidekick setup --backend {name}` ausführen."
                ));
                return Ok(None);
            }
        }
    }
}

/// Read a secret until it is non-empty and has no control characters (three tries).
/// `None`: given up.
fn read_valid_secret(p: &mut dyn Prompter, prompt: &str) -> Result<Option<Token>> {
    for _ in 0..3 {
        let token = p.secret(prompt)?;
        if token.expose().is_empty() {
            p.say("Die Eingabe war leer. Bitte den Schlüssel einfügen.");
        } else if token.expose().chars().any(char::is_control) {
            p.say("Die Eingabe enthält Zeilenumbrüche oder Steuerzeichen. Bitte als eine Zeile einfügen.");
        } else {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

enum Verified {
    Keep,
    Retry,
    Skip,
}

/// For an opencode backend (KI:connect): `GET {base_url}/models` with the key; 401/403
/// means rejected. Other backends are not checked.
fn verify_key(
    p: &mut dyn Prompter,
    s: &dyn System,
    prof: Profile,
    backend: &Backend,
    token: &Token,
) -> Result<Verified> {
    let Some(url) = crate::harness::opencode::probe_url(backend) else {
        return Ok(Verified::Keep);
    };
    if !matches!(
        backend.kind,
        BackendKind::OpenCode {
            provider: crate::backends::OpencodeProvider::OpenaiCompatible,
            ..
        }
    ) || !p.confirm("Schlüssel jetzt online prüfen?", true)?
    {
        return Ok(Verified::Keep);
    }
    match s.probe_get(&url, Some(token.expose())) {
        ProbeResponse::Status { code, .. } if code == 401 || code == 403 => {
            let hint = match prof {
                Profile::Kiconnect => " – erstelle einen neuen auf https://chat.kiconnect.nrw",
                _ => "",
            };
            p.say(&format!("Schlüssel abgelehnt (HTTP {code}){hint}."));
            Ok(if p.confirm("Neuen Schlüssel eingeben?", true)? {
                Verified::Retry
            } else {
                Verified::Skip
            })
        }
        ProbeResponse::Status { code, .. } => {
            p.say(&format!("Schlüssel geprüft: HTTP {code}, angenommen."));
            Ok(Verified::Keep)
        }
        ProbeResponse::Failed(why) => {
            p.say(&format!(
                "Prüfung nicht möglich ({}); der Schlüssel wird trotzdem gespeichert. Später \
                 prüfen mit `agent-sidekick backends test {}`.",
                redact_all(&why),
                backend.name
            ));
            Ok(Verified::Keep)
        }
    }
}

fn secret_instructions(prof: Profile, backend: &Backend, os: Os) -> String {
    match prof {
        Profile::Kiconnect => {
            "Melde dich auf https://chat.kiconnect.nrw mit deinem Hochschulkonto \
             an und erstelle dort einen API-Schlüssel. Er gilt einen Monat."
                .to_owned()
        }
        Profile::Claude => {
            let dir = match &backend.kind {
                BackendKind::ClaudeOauth { config_dir } => config_dir.display().to_string(),
                _ => "~/.claude-sidekick".to_owned(),
            };
            let cmd = match os {
                Os::Windows => format!(
                    "  PowerShell:  $env:CLAUDE_CONFIG_DIR=\"{dir}\"; claude setup-token\n\
                     \x20 Git Bash:    CLAUDE_CONFIG_DIR=\"{dir}\" claude setup-token"
                ),
                Os::Unix => format!("  CLAUDE_CONFIG_DIR=\"{dir}\" claude setup-token"),
            };
            format!(
                "Erzeuge einen Token für das ZWEITE Claude-Konto. Öffne dafür ein separates \
                 Terminal und führe aus:\n{cmd}\nMelde dich im Browser mit dem zweiten Konto an \
                 (nicht mit dem, das du in VS Code nutzt). Kopiere den ausgegebenen Token; er \
                 beginnt mit {EXPECTED_PREFIX}."
            )
        }
        Profile::Gemini => "Erstelle einen Gemini-API-Schlüssel auf \
             https://aistudio.google.com/apikey (Google-Konto nötig)."
            .to_owned(),
    }
}

fn example_backend(profiles: &[Profile]) -> &'static str {
    if profiles.contains(&Profile::Kiconnect) {
        "kiconnect"
    } else {
        profiles
            .first()
            .map(|p| p.backend_name())
            .unwrap_or("kiconnect")
    }
}

#[cfg(test)]
mod tests;
