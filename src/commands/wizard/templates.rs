//! Embedded config.toml templates of the setup wizard and the text operations on them
//! (fill in, mark the default, merge into an existing file).

use std::path::Path;

use super::Profile;
use crate::backends::{self, Backends, ResolveDefaults};
use crate::config::Config;

pub const HEADER: &str = include_str!("../../../templates/header.toml");
pub const KICONNECT: &str = include_str!("../../../templates/kiconnect.toml");
pub const SIDEKICK: &str = include_str!("../../../templates/sidekick.toml");
pub const GEMINI: &str = include_str!("../../../templates/gemini.toml");

/// Placeholders of the Gemini template (never `{prompt}`-style: those are command
/// placeholders and are rejected in `command[0]`).
pub const NODE_PLACEHOLDER: &str = "{{NODE}}";
pub const GEMINI_ENTRY_PLACEHOLDER: &str = "{{GEMINI_ENTRY}}";

/// Paths the Gemini template needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiPaths<'a> {
    pub node: &'a Path,
    pub entry: &'a Path,
}

/// A TOML basic string (quoted, `\` and `"` and control characters escaped) for `value`.
pub fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The table of `profile`, filled in, with `default = true` right after its header when
/// `is_default`. Gemini needs `gemini` (the node and entry paths).
pub fn section(
    profile: Profile,
    is_default: bool,
    gemini: Option<&GeminiPaths<'_>>,
) -> Result<String, String> {
    let mut text = match profile {
        Profile::Kiconnect => KICONNECT.to_owned(),
        Profile::Claude => SIDEKICK.to_owned(),
        Profile::Gemini => {
            let paths = gemini
                .ok_or("internal error: the Gemini template needs the node and Gemini CLI paths")?;
            GEMINI
                .replace(
                    NODE_PLACEHOLDER,
                    &toml_string(&paths.node.to_string_lossy()),
                )
                .replace(
                    GEMINI_ENTRY_PLACEHOLDER,
                    &toml_string(&paths.entry.to_string_lossy()),
                )
        }
    };
    if is_default {
        let header = format!("[backends.{}]\n", profile.backend_name());
        let at = text
            .find(&header)
            .ok_or("internal error: template without its table header")?;
        text.insert_str(at + header.len(), "default = true\n");
    }
    Ok(text)
}

/// A complete new config.toml: the header and the given sections in order.
pub fn fresh_config(sections: &[String]) -> String {
    let mut out = HEADER.to_owned();
    for s in sections {
        out.push_str(s);
    }
    out
}

/// `existing` unchanged (byte for byte) with the sections appended; a missing final line
/// break is added first.
pub fn merged_config(existing: &str, sections: &[String]) -> String {
    let mut out = existing.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for s in sections {
        out.push_str(s);
    }
    out
}

/// Parse and validate config text exactly like the server does ([`Config`] +
/// [`backends::resolve_with`]). The error is a user-facing message.
pub fn validate(text: &str, defaults: &ResolveDefaults) -> Result<(Config, Backends), String> {
    let cfg: Config = toml::from_str(text).map_err(|e| format!("config.toml: {e}"))?;
    let backends = backends::resolve_with(&cfg, defaults)?;
    Ok((cfg, backends))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{BackendKind, OpencodeProvider};
    use crate::token::SecretId;
    use std::path::PathBuf;

    fn defaults(dir: &Path) -> ResolveDefaults {
        ResolveDefaults {
            home: Some(dir.join("home")),
            legacy_agent_config_dir: Some(dir.join("home").join(".claude-sidekick")),
            config_dir: Some(dir.join("cfg")),
        }
    }

    fn gemini_paths(dir: &Path) -> (PathBuf, PathBuf) {
        // Backslashes and a quote on purpose: they must survive the TOML escaping.
        (
            dir.join("node dir").join("node.exe"),
            dir.join("npm\\root")
                .join("gemini \"cli\"")
                .join("gemini.js"),
        )
    }

    fn all_sections(dir: &Path, default: Profile) -> Vec<String> {
        let (node, entry) = gemini_paths(dir);
        let g = GeminiPaths {
            node: &node,
            entry: &entry,
        };
        Profile::ALL
            .iter()
            .map(|&p| section(p, p == default, Some(&g)).unwrap())
            .collect()
    }

    #[test]
    fn every_template_alone_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let (node, entry) = gemini_paths(dir.path());
        let g = GeminiPaths {
            node: &node,
            entry: &entry,
        };
        for p in Profile::ALL {
            let text = fresh_config(&[section(p, true, Some(&g)).unwrap()]);
            let (_, b) = validate(&text, &defaults(dir.path()))
                .unwrap_or_else(|e| panic!("{p:?}: {e}\n{text}"));
            assert_eq!(b.len(), 1);
            let backend = b.default_backend();
            assert_eq!(backend.name, p.backend_name());
            assert_eq!(backend.secret_id(), Some(p.secret_id()));
        }
    }

    #[test]
    fn templates_have_the_expected_content() {
        let dir = tempfile::tempdir().unwrap();
        let text = fresh_config(&all_sections(dir.path(), Profile::Kiconnect));
        let (_, b) = validate(&text, &defaults(dir.path())).unwrap();
        assert_eq!(b.names(), ["gemini", "kiconnect", "sidekick"]);
        assert_eq!(b.default_backend().name, "kiconnect");

        let k = b.get("kiconnect").unwrap();
        let BackendKind::OpenCode {
            provider, base_url, ..
        } = &k.kind
        else {
            panic!("{k:?}")
        };
        assert_eq!(*provider, OpencodeProvider::OpenaiCompatible);
        assert_eq!(
            base_url.as_deref(),
            Some("https://chat.kiconnect.nrw/api/v1")
        );
        assert_eq!(
            k.models.as_deref().unwrap(),
            ["openai-gpt-oss-120b", "qwen-qwen3-8-27b"]
        );
        assert_eq!(k.model.as_deref(), Some("openai-gpt-oss-120b"));
        assert!(k
            .description
            .starts_with("OpenCode agent via KI:connect.nrw"));

        let s = b.get("sidekick").unwrap();
        assert!(matches!(s.kind, BackendKind::ClaudeOauth { .. }), "{s:?}");
        assert_eq!(s.secret_id(), Some(SecretId::legacy()));

        let g = b.get("gemini").unwrap();
        let BackendKind::Command {
            command,
            secret_env,
            prompt_via_stdin,
        } = &g.kind
        else {
            panic!("{g:?}")
        };
        let (node, entry) = gemini_paths(dir.path());
        assert_eq!(command[0], node.to_string_lossy());
        assert_eq!(command[1], entry.to_string_lossy());
        assert!(command.contains(&"--prompt={prompt}".to_owned()));
        assert!(command.contains(&"--model={model}".to_owned()));
        assert!(!prompt_via_stdin);
        assert_eq!(secret_env.as_deref(), Some("GEMINI_API_KEY"));
        assert_eq!(g.secret_id(), Some(SecretId::Command("gemini".into())));
        assert_eq!(g.model.as_deref(), Some("auto"));
    }

    #[test]
    fn every_default_choice_resolves() {
        let dir = tempfile::tempdir().unwrap();
        for d in Profile::ALL {
            let text = fresh_config(&all_sections(dir.path(), d));
            let (_, b) = validate(&text, &defaults(dir.path())).unwrap();
            assert_eq!(b.default_backend().name, d.backend_name());
            assert_eq!(text.matches("default = true").count(), 1);
        }
    }

    #[test]
    fn gemini_without_paths_is_an_error() {
        assert!(section(Profile::Gemini, false, None).is_err());
    }

    #[test]
    fn merge_into_the_example_config() {
        let dir = tempfile::tempdir().unwrap();
        let example = include_str!("../../../examples/config.toml");
        let (cfg, _) = validate(example, &defaults(dir.path())).unwrap();
        let existing = cfg.backends.unwrap();
        // The example has sidekick and kiconnect already: only gemini is added.
        let (node, entry) = gemini_paths(dir.path());
        let g = GeminiPaths {
            node: &node,
            entry: &entry,
        };
        assert!(existing.contains_key("kiconnect") && existing.contains_key("sidekick"));
        let add = vec![section(Profile::Gemini, false, Some(&g)).unwrap()];
        let merged = merged_config(example, &add);
        assert!(merged.starts_with(example));
        let (_, b) = validate(&merged, &defaults(dir.path())).unwrap();
        assert!(b.get("gemini").is_some());
        assert_eq!(b.default_backend().name, "sidekick");
        // The example's kiconnect stays the aider command backend.
        assert!(matches!(
            b.get("kiconnect").unwrap().kind,
            BackendKind::Command { .. }
        ));
    }

    #[test]
    fn merge_adds_a_missing_line_break() {
        assert_eq!(merged_config("a = 1", &["\nb\n".into()]), "a = 1\n\nb\n");
        assert_eq!(merged_config("a = 1\n", &[]), "a = 1\n");
    }

    #[test]
    fn toml_string_escapes() {
        assert_eq!(toml_string(r"C:\a b\x.exe"), r#""C:\\a b\\x.exe""#);
        assert_eq!(toml_string("q\"\n"), r#""q\"\n""#);
        let v: toml::Value =
            toml::from_str(&format!("k = {}", toml_string("a\\\"b\u{1}"))).unwrap();
        assert_eq!(v["k"].as_str(), Some("a\\\"b\u{1}"));
    }
}
