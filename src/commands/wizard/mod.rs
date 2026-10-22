//! `agent-sidekick setup` without flags in an interactive terminal: a guided setup.
//!
//! Steps: introduction, prerequisites (Claude Code or Codex as the MCP host: without
//! Claude Code the wizard registers only in Codex, without either it aborts; Node.js,
//! OpenCode), profiles
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
use crate::backends::{
    Backend, BackendKind, Backends, CodexAuth, IMPLICIT_BACKEND, OLD_IMPLICIT_BACKEND,
};
use crate::config::Config;
use crate::locate::Os;
use crate::redact::{redact_all, MIN_SECRET_LEN};
use crate::token::{self, SecretId, Token, EXPECTED_PREFIX};
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
    Codex,
    Groq,
}

impl Profile {
    /// In the order the wizard offers them.
    pub const ALL: [Profile; 5] = [
        Profile::Kiconnect,
        Profile::Claude,
        Profile::Gemini,
        Profile::Codex,
        Profile::Groq,
    ];

    pub fn backend_name(self) -> &'static str {
        match self {
            Profile::Kiconnect => "kiconnect",
            Profile::Claude => "claude",
            Profile::Gemini => "gemini",
            Profile::Codex => "codex",
            Profile::Groq => "groq",
        }
    }

    /// Names under which an existing backend counts as this profile: the Claude profile also
    /// matches `sidekick`, its backend name before v0.6.
    pub fn backend_names(self) -> &'static [&'static str] {
        match self {
            Profile::Claude => &[IMPLICIT_BACKEND, OLD_IMPLICIT_BACKEND],
            Profile::Kiconnect => &["kiconnect"],
            Profile::Gemini => &["gemini"],
            Profile::Codex => &["codex"],
            Profile::Groq => &["groq"],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Profile::Kiconnect => "KI:connect.nrw (Hochschul-Kontingent, kostenlos)",
            Profile::Claude => "Claude-Zweitkonto",
            Profile::Gemini => "Gemini",
            Profile::Codex => "OpenAI Codex (ChatGPT-Abo oder API-Key)",
            Profile::Groq => "Groq (kostenlos, mit Limits)",
        }
    }

    /// The stored secret of the backend the template creates (Codex: with `auth =
    /// "chatgpt"`, which needs none; with `api-key` it is `backend-codex`).
    pub fn secret_id(self) -> Option<SecretId> {
        match self {
            Profile::Kiconnect => Some(SecretId::Backend("kiconnect".into())),
            Profile::Claude => Some(SecretId::legacy()),
            Profile::Gemini => Some(SecretId::Command("gemini".into())),
            Profile::Codex => None,
            Profile::Groq => Some(SecretId::Backend("groq".into())),
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
    agent-sidekick lässt Claude Code (oder Codex) Aufgaben an einen zweiten Agenten abgeben, \
    zum Beispiel an KI:connect.nrw, Groq, ein zweites Claude-Konto, Gemini oder OpenAI Codex.\n\
    Der Assistent prüft die Voraussetzungen, schreibt die Konfiguration, speichert deine \
    Schlüssel sicher im Schlüsselbund und meldet den Server bei Claude Code an (auf Wunsch \
    auch bei Codex; ohne Claude Code nur bei Codex).";

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
            Some(path)
        }
        Err(e) => {
            p.say(&format!(
                "  Claude Code: nicht gefunden ({})",
                redact_all(&e)
            ));
            None
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
        None => p.say("  OpenCode: nicht gefunden (nur für KI:connect.nrw und Groq nötig)"),
    }
    let mut codex = s.find_codex();
    match &codex {
        Some(path) => p.say(&format!(
            "  Codex: {} ({})",
            path.display(),
            version_text(s, Tool::Codex, path)
        )),
        None => p.say("  Codex: nicht gefunden (nur für OpenAI Codex nötig)"),
    }
    if claude.is_none() {
        if codex.is_none() {
            bail!("{}", no_host_message(s.os()));
        }
        p.say(&format!("\n{CODEX_ONLY_NOTE}"));
    }

    // 2. Profiles (the second Claude account needs the claude CLI).
    let available: Vec<Profile> = Profile::ALL
        .into_iter()
        .filter(|&x| x != Profile::Claude || claude.is_some())
        .collect();
    let mut profiles = choose_profiles(p, &available)?;

    // 3. Tools for the profiles (nothing is written before this step succeeds).
    let needs_node = (needs_opencode(&profiles) && opencode.is_none())
        || (profiles.contains(&Profile::Codex) && codex.is_none())
        || profiles.contains(&Profile::Gemini);
    let node = match node {
        Some(n) => Some(n),
        None if needs_node => {
            p.say(&node_instructions(s.os()));
            bail!(
                "Node.js fehlt (gebraucht für {}). Nächster Schritt: Node.js installieren, ein \
                 neues Terminal öffnen (nutzt du das Terminal in VS Code: VS Code komplett neu \
                 starten). {RESUME} Es wurde nichts geändert.",
                node_users(&profiles, opencode.is_none(), codex.is_none())
            );
        }
        None => None,
    };
    if needs_opencode(&profiles) && opencode.is_none() {
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
                "Hinweis: {} OpenCode. Installiere es später mit \
                 `npm install -g {}` und prüfe dann mit `agent-sidekick doctor`.",
                opencode_users(&profiles),
                npm::OPENCODE_PACKAGE
            ));
        }
    }
    if profiles.contains(&Profile::Codex) && codex.is_none() {
        let node = node.as_deref().expect("checked above");
        if offer_install(p, s, node, npm::CODEX_PACKAGE, "Codex")? {
            codex = s.find_codex();
            match &codex {
                Some(path) => p.say(&format!("Codex installiert: {}", path.display())),
                None => p.say(
                    "Codex wurde installiert, aber noch nicht gefunden. Nächster Schritt: ein \
                     neues Terminal öffnen und `agent-sidekick doctor` ausführen.",
                ),
            }
        } else {
            p.say(&format!(
                "Hinweis: OpenAI Codex braucht die Codex-CLI. Installiere sie später mit \
                 `npm install -g {}` und prüfe dann mit `agent-sidekick doctor`.",
                npm::CODEX_PACKAGE
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
                p.say(GEMINI_WARNING);
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
    // config without [backends]) the implicit `claude`, which a [backends] section would
    // otherwise remove.
    let mut add: Vec<Profile> = Vec::new();
    for &prof in &profiles {
        let exists = existing_backends
            .as_ref()
            .and_then(|m| prof.backend_names().iter().find(|n| m.contains_key(**n)));
        if let Some(name) = exists {
            p.say(&format!(
                "Backend `{name}` steht schon in der config.toml und bleibt unverändert."
            ));
        } else {
            add.push(prof);
        }
    }
    let implicit_sidekick = merging
        && existing_backends.is_none()
        && !add.contains(&Profile::Claude)
        && !add.is_empty();
    if implicit_sidekick && claude.is_none() {
        p.say(
            "Die bisherige Konfiguration nutzt das eingebaute Backend `claude` (zweites \
             Claude-Konto). Ohne Claude Code kann es nicht laufen und wird deshalb nicht \
             übernommen.",
        );
    } else if implicit_sidekick {
        p.say(
            "Die bisherige Konfiguration nutzt das eingebaute Backend `claude` (zweites \
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

    // How a new Codex backend pays: the ChatGPT plan (login) or an OpenAI API key.
    let codex_auth = if add.contains(&Profile::Codex) {
        let options = [
            "ChatGPT-Abo: einmal mit deinem ChatGPT-Konto anmelden (Plus, Pro, Business, Edu, \
             Enterprise)"
                .to_owned(),
            "OpenAI-API-Schlüssel: Abrechnung pro Token über platform.openai.com".to_owned(),
        ];
        match p.choose_one("\nWie soll OpenAI Codex abgerechnet werden?", &options, 0)? {
            0 => CodexAuth::Chatgpt,
            _ => CodexAuth::ApiKey,
        }
    } else {
        CodexAuth::Chatgpt
    };

    // 6. Build and validate the new config before anything is asked or written.
    let mut sections = Vec::new();
    for &prof in &add {
        sections.push(
            templates::section(
                prof,
                default == Some(prof),
                gemini_paths.as_ref(),
                codex_auth,
            )
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
        let Some(backend) = prof.backend_names().iter().find_map(|n| backends.get(n)) else {
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
                secret_label(id),
                redact_all(&format!("{e:#}"))
            )
        })?;
        p.say(&format!("{} gespeichert {source}.", secret_label(id)));
    }
    drop(secrets);

    // 8b. The ChatGPT login of a Codex backend (it lives in the backend's own CODEX_HOME).
    if profiles.contains(&Profile::Codex) {
        if let Some(b) = backends.get(Profile::Codex.backend_name()) {
            codex_login(p, s, b)?;
        }
    }

    // 9. Registration with Claude Code (skipped without the claude CLI).
    p.say("\nAnmeldung bei Claude Code:");
    match &claude {
        Some(claude) => {
            if let Err(e) = s.register(claude) {
                p.say(&format!(
                    "Die Anmeldung bei Claude Code ist fehlgeschlagen: {}\nNächster Schritt: \
                     `agent-sidekick setup --register-only` ausführen.",
                    redact_all(&format!("{e:#}"))
                ));
            }
        }
        None => p.say(
            "Übersprungen: Claude Code ist nicht installiert. Installierst du Claude Code \
             später, führe `agent-sidekick setup --register-only` aus.",
        ),
    }

    // 9b. Registration with Codex (only when the Codex CLI is installed; without Claude
    // Code it is the only host, so it is not asked).
    let mut in_codex = false;
    let codex_wanted = match (&claude, s.find_codex().or(codex)) {
        (_, None) => false,
        (None, Some(_)) => {
            p.say("\nAnmeldung bei Codex:");
            true
        }
        (Some(_), Some(_)) => p.confirm(
            "\nAuch in Codex anmelden? Dann kannst du agent-sidekick auch in der Codex-CLI \
             nutzen.",
            true,
        )?,
    };
    if codex_wanted {
        match s.register_codex(claude.as_deref()) {
            Ok(()) => in_codex = true,
            Err(e) => p.say(&format!(
                "Die Anmeldung bei Codex ist fehlgeschlagen: {}\nNächster Schritt: \
                 `agent-sidekick setup --codex` ausführen.",
                redact_all(&format!("{e:#}"))
            )),
        }
    }

    // 10. Doctor.
    p.say("\nPrüfung (agent-sidekick doctor):");
    p.say(s.doctor().trim_end());

    // 11. Finish.
    let example = example_backend(&profiles);
    if claude.is_some() {
        p.say(&format!(
            "\nFertig. Nächste Schritte:\n\
             1. VS Code neu laden (Befehlspalette: „Developer: Reload Window“), dann in Claude Code \
             /mcp prüfen: `{APP_NAME}` sollte als verbunden erscheinen.\n\
             2. Ausprobieren, z. B. mit dem Prompt: „Lass {example} die README zusammenfassen“."
        ));
        if in_codex {
            p.say(&format!(
                "In Codex: eine neue Sitzung starten (`codex`); `/mcp` zeigt `{APP_NAME}`. Vor \
                 jedem sidekick_run fragt Codex nach deiner Zustimmung."
            ));
        }
    } else {
        p.say(&format!(
            "\nFertig. Nächste Schritte:\n\
             1. Eine neue Codex-Sitzung starten (`codex`; die Codex-Erweiterung in VS Code nach \
             „Developer: Reload Window“); `/mcp` zeigt `{APP_NAME}`. Vor jedem sidekick_run \
             fragt Codex nach deiner Zustimmung.\n\
             2. Ausprobieren, z. B. mit dem Prompt: „Lass {example} die README zusammenfassen“."
        ));
    }
    if profiles.contains(&Profile::Kiconnect) {
        p.say(
            "Hinweis: Ein KI:connect-Schlüssel gilt einen Monat und verlängert sich bei jeder \
             Anmeldung im Webinterface (https://chat.kiconnect.nrw) um einen weiteren Monat. Ist er \
             abgelaufen, erzeuge dort unter „API Key Management“ mit „Rotate“ einen neuen und \
             speichere ihn mit `agent-sidekick setup --backend kiconnect`.",
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

fn node_users(profiles: &[Profile], opencode_missing: bool, codex_missing: bool) -> String {
    let mut users = Vec::new();
    if opencode_missing && needs_opencode(profiles) {
        users.push("die Installation von OpenCode");
    }
    if codex_missing && profiles.contains(&Profile::Codex) {
        users.push("die Installation von Codex");
    }
    if profiles.contains(&Profile::Gemini) {
        users.push("die Gemini CLI");
    }
    users.join(" und ")
}

fn needs_opencode(profiles: &[Profile]) -> bool {
    profiles
        .iter()
        .any(|x| matches!(x, Profile::Kiconnect | Profile::Groq))
}

fn opencode_users(profiles: &[Profile]) -> String {
    match (
        profiles.contains(&Profile::Kiconnect),
        profiles.contains(&Profile::Groq),
    ) {
        (true, true) => "KI:connect.nrw und Groq brauchen".to_owned(),
        (false, true) => "Groq braucht".to_owned(),
        _ => "KI:connect.nrw braucht".to_owned(),
    }
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
         Öffne danach ein neues Terminal. Nutzt du das Terminal in VS Code, starte VS Code \
         komplett neu: Ein neues Terminal in VS Code kennt Node.js sonst noch nicht. {RESUME}"
    )
}

/// The official native installer of the Claude Code CLI for this platform (the same
/// commands as the `claude` harness's install hint, from code.claude.com/docs/en/setup).
pub fn claude_install_command(os: Os) -> &'static str {
    match os {
        Os::Windows => "irm https://claude.ai/install.ps1 | iex",
        Os::Unix => "curl -fsSL https://claude.ai/install.sh | bash",
    }
}

/// Shown when Claude Code is missing but the Codex CLI is installed.
pub const CODEX_ONLY_NOTE: &str = "Claude Code nicht gefunden – agent-sidekick wird nur in \
    Codex angemeldet. Installierst du Claude Code später, führe agent-sidekick setup \
    --register-only aus.";

/// The abort message when neither Claude Code nor the Codex CLI is installed: agent-sidekick
/// needs at least one host to register with.
fn no_host_message(os: Os) -> String {
    let shell = match os {
        Os::Windows => "in PowerShell",
        Os::Unix => "im Terminal",
    };
    format!(
        "Weder Claude Code noch die Codex-CLI wurde gefunden. agent-sidekick braucht \
         mindestens eines davon, um sich dort anzumelden; die VS-Code-Erweiterung allein reicht \
         nicht. Nächster Schritt, eins von beiden:\n\
         \x20 - Claude Code installieren ({shell}): {}\n\
         \x20   (weitere Wege: https://code.claude.com/docs/en/setup)\n\
         \x20 - Codex installieren (braucht Node.js): npm install -g {}\n\
         \x20   danach einmal `codex login` mit deinem Konto.\n\
         Öffne danach ein neues Terminal; nutzt du das Terminal in VS Code, starte VS Code \
         komplett neu. Liegt claude woanders, setze den Pfad in AGENT_SIDEKICK_CLAUDE. {RESUME} \
         Es wurde nichts geändert.",
        claude_install_command(os),
        npm::CODEX_PACKAGE
    )
}

/// Shown when the Gemini profile is set up (see docs/REFERENCE.md, Command backends).
pub const GEMINI_WARNING: &str = "Achtung, Gemini: Das Backend startet die Gemini CLI mit \
    --skip-trust, sonst bricht sie in nicht vertrauten Ordnern ab. Sie vertraut damit jedem \
    Projektordner: Eine \
    `.env`, `.gemini/.env` oder `.gemini/settings.json` im Projekt kann deinen Gemini-Schlüssel an \
    einen fremden Server schicken oder Code ausführen. Nutze Gemini nur in Ordnern, denen du \
    vertraust, und einen Schlüssel, den du widerrufen kannst.";

fn choose_profiles(p: &mut dyn Prompter, available: &[Profile]) -> Result<Vec<Profile>> {
    let options: Vec<String> = available.iter().map(|x| x.label().to_owned()).collect();
    for _ in 0..3 {
        let picked = p.choose_many(
            "\nWelche Profile möchtest du einrichten? (mehrere möglich)",
            &options,
            &[0],
        )?;
        if !picked.is_empty() {
            return Ok(picked.into_iter().map(|i| available[i]).collect());
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
        if let Some(BackendKind::ClaudeOauth { config_dir, .. }) =
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
        // A ChatGPT login is made after the config is written (see `codex_login`).
        let chatgpt = matches!(
            backend.kind,
            BackendKind::Codex {
                auth: CodexAuth::Chatgpt,
                ..
            }
        );
        if !chatgpt {
            p.say(&format!(
                "Backend `{name}`: kein gespeicherter Schlüssel nötig (z. B. Umgebungsvariable)."
            ));
        }
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
        Profile::Codex => "OpenAI-API-Schlüssel einfügen (Eingabe unsichtbar): ",
        Profile::Groq => "Groq-API-Schlüssel einfügen (Eingabe unsichtbar): ",
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
        // Same rule as `setup` (token::check_*_format): short values are not redacted.
        if matches!(
            token::check_command_secret_format(token.expose()),
            token::FormatCheck::Warn(_)
        ) {
            p.say(&format!(
                "Warnung: Der Schlüssel ist kürzer als {MIN_SECRET_LEN} Zeichen und wird deshalb \
                 nicht aus Logs und Ausgaben entfernt. Er wird trotzdem gespeichert."
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

/// German name of a stored secret (the `Display` of [`SecretId`] is the English one of
/// `setup` and `doctor`).
fn secret_label(id: &SecretId) -> String {
    match id {
        SecretId::Oauth(n) => format!("OAuth-Token von Backend `{n}`"),
        SecretId::Endpoint(n) => format!("API-Schlüssel von Backend `{n}`"),
        SecretId::Command(n) => format!("Schlüssel von Backend `{n}`"),
        SecretId::Backend(n) => format!("Provider-Schlüssel von Backend `{n}`"),
    }
}

/// Read a secret until it is non-empty and has no control characters (three tries).
/// `None`: given up, and the user chose to go on without it. After three bad inputs the
/// wizard asks on a line prompt, so a closed input (EOF: the hidden prompt then reads an
/// empty string) aborts there instead of silently going on; nothing is written yet.
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
    if !p.confirm("Ohne diesen Schlüssel weitermachen?", true)? {
        bail!(
            "Abgebrochen. Es wurde nichts geändert. Nächster Schritt: den Schlüssel bereitlegen \
             und `agent-sidekick setup` erneut ausführen."
        );
    }
    Ok(None)
}

enum Verified {
    Keep,
    Retry,
    Skip,
}

/// For an opencode backend with an OpenAI-compatible provider (KI:connect) or a codex
/// api-key backend: `GET {base_url}/models` with the key; 401/403 means rejected. Other
/// backends are not checked.
fn verify_key(
    p: &mut dyn Prompter,
    s: &dyn System,
    prof: Profile,
    backend: &Backend,
    token: &Token,
) -> Result<Verified> {
    let url = match &backend.kind {
        BackendKind::OpenCode {
            provider: crate::backends::OpencodeProvider::OpenaiCompatible,
            ..
        } => crate::harness::opencode::probe_url(backend),
        BackendKind::Codex { .. } => crate::harness::codex::probe_url(backend),
        _ => None,
    };
    let Some(url) = url else {
        return Ok(Verified::Keep);
    };
    if !p.confirm("Schlüssel jetzt online prüfen?", true)? {
        return Ok(Verified::Keep);
    }
    match s.probe_get(&url, Some(token.expose())) {
        ProbeResponse::Status { code, .. } if code == 401 || code == 403 => {
            let hint = match prof {
                Profile::Kiconnect => " – erstelle einen neuen auf https://chat.kiconnect.nrw",
                Profile::Codex => " – prüfe ihn auf https://platform.openai.com/api-keys",
                Profile::Groq => " – erstelle einen neuen auf https://console.groq.com/keys",
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
            "Melde dich auf https://chat.kiconnect.nrw/app mit deinem Hochschulkonto \
             an. Klicke unten links auf deinen Namen, wähle „API Key Management“ und dann \
             „Create Key“. Kopiere den Schlüssel."
                .to_owned()
        }
        Profile::Claude => {
            let dir = match &backend.kind {
                BackendKind::ClaudeOauth { config_dir, .. } => config_dir.display().to_string(),
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
        Profile::Codex => "Erstelle einen OpenAI-API-Schlüssel auf \
             https://platform.openai.com/api-keys (Abrechnung pro Token über dein \
             OpenAI-Konto, unabhängig von einem ChatGPT-Abo)."
            .to_owned(),
        Profile::Groq => "Erstelle einen Groq-API-Schlüssel auf https://console.groq.com/keys \
             (kostenlos, ohne Kreditkarte). Die kostenlose Stufe ist eng begrenzt (etwa 8000 \
             Tokens pro Minute und Modell), nutze Groq daher nur für kleine Aufgaben."
            .to_owned(),
    }
}

/// The ChatGPT login of a codex backend with `auth = "chatgpt"`: create its `CODEX_HOME`
/// with the generated config, show the `codex login` command for exactly that directory,
/// wait for the user and check that the login file exists (never read). Not logged in is
/// no error: the final steps name the command again.
fn codex_login(p: &mut dyn Prompter, s: &dyn System, backend: &Backend) -> Result<()> {
    let BackendKind::Codex {
        auth: CodexAuth::Chatgpt,
        ..
    } = &backend.kind
    else {
        return Ok(());
    };
    let name = &backend.name;
    let home = s
        .prepare_codex_home(backend)
        .map_err(|e| anyhow::anyhow!("{e}. {DIR_NEXT_STEP}"))?;
    let auth = home.join(crate::harness::codex::AUTH_FILE);
    p.say(&format!(
        "\n— {} (Backend `{name}`) —",
        Profile::Codex.label()
    ));
    if s.file_exists(&auth) {
        p.say(&format!(
            "Für `{name}` ist schon eine ChatGPT-Anmeldung vorhanden ({}).",
            auth.display()
        ));
        return Ok(());
    }
    p.say(&format!(
        "Melde dich jetzt einmal mit deinem ChatGPT-Konto an. Die Anmeldung gilt nur für \
         agent-sidekick und liegt in {} (dein eigenes ~/.codex bleibt unberührt). Führe in \
         einem separaten Terminal aus:\n{}\nIm Browser mit dem ChatGPT-Konto anmelden, dessen \
         Abo genutzt werden soll.",
        home.display(),
        crate::harness::codex::login_commands(&home)
    ));
    let _ = p.confirm("Anmeldung erledigt?", true)?;
    if s.file_exists(&auth) {
        p.say("ChatGPT-Anmeldung gefunden.");
    } else {
        p.say(&format!(
            "Noch keine Anmeldung gefunden ({} fehlt). Nächster Schritt: den Befehl oben \
             ausführen und mit `agent-sidekick backends test {name}` prüfen.",
            auth.display()
        ));
    }
    Ok(())
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
