//! Removes secrets from text before it reaches logs, errors or tool responses.

use std::fmt;
use std::sync::Arc;

pub const REDACTED: &str = "[REDACTED]";
const TOKEN_PREFIX: &str = "sk-ant-";

/// Secrets shorter than this are not redacted: placeholder keys of local servers (e.g.
/// `ollama`) would otherwise mangle every occurrence of that word in the agent's output.
/// `sk-ant-…` tokens are always redacted regardless of this limit.
pub const MIN_SECRET_LEN: usize = 8;

/// Replace every occurrence of `secret` (if non-empty) and every `sk-ant-…` token.
pub fn redact(text: &str, secret: Option<&str>) -> String {
    let text = match secret {
        Some(s) if !s.is_empty() => text.replace(s, REDACTED),
        _ => text.to_owned(),
    };
    redact_all(&text)
}

/// Replace every `sk-ant-…` looking token with [`REDACTED`].
pub fn redact_all(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(TOKEN_PREFIX) {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + TOKEN_PREFIX.len()..];
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(after.len());
        out.push_str(REDACTED);
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// A set of secret values to redact (tokens, API keys). Values shorter than
/// [`MIN_SECRET_LEN`] are ignored; `Debug` never shows the values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets(Arc<Vec<String>>);

impl Secrets {
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut v: Vec<String> = Vec::new();
        for value in values {
            let value = value.as_ref().trim();
            v.push(value.to_owned());
            // Output is redacted line by line: also register every line of a multi-line
            // value (defence in depth; storing such values is rejected).
            if value.contains(['\n', '\r']) {
                v.extend(value.lines().map(|l| l.trim().to_owned()));
            }
        }
        v.retain(|s| s.chars().count() >= MIN_SECRET_LEN);
        // Longest first, so a secret containing another one is replaced as a whole.
        v.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        v.dedup();
        Self(Arc::new(v))
    }

    pub fn none() -> Self {
        Self::default()
    }

    /// The secret values (for cut-point computations; never print them).
    pub fn values(&self) -> &[String] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Replace every secret and every `sk-ant-…` token in `text`.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for s in self.0.iter() {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), REDACTED);
            }
        }
        redact_all(&out)
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({} redacted)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_explicit_secret() {
        assert_eq!(redact("a hunter2 b", Some("hunter2")), "a [REDACTED] b");
    }

    #[test]
    fn redacts_token_pattern() {
        let s = "token=sk-ant-oat01-AbC_d-9 end";
        assert_eq!(redact_all(s), "token=[REDACTED] end");
    }

    #[test]
    fn leaves_clean_text_alone() {
        assert_eq!(redact("nothing here", None), "nothing here");
        assert_eq!(redact("x", Some("")), "x");
    }

    #[test]
    fn secrets_set_redacts_all_and_skips_short_placeholders() {
        let s = Secrets::new(["ollama", "longsecretvalue1", "otherkey-123456", "  "]);
        assert_eq!(s.values().len(), 2);
        let text = "ollama says longsecretvalue1 and otherkey-123456 and sk-ant-api03-xyz";
        assert_eq!(
            s.redact(text),
            "ollama says [REDACTED] and [REDACTED] and [REDACTED]"
        );
        assert!(!format!("{s:?}").contains("longsecret"));
    }

    #[test]
    fn secrets_multiline_values_are_redacted_per_line() {
        let s = Secrets::new(["first-line-secret\nsecond-line-secret\r\nab"]);
        let out = s.redact("A first-line-secret B\nC second-line-secret D ab");
        assert_eq!(out, "A [REDACTED] B\nC [REDACTED] D ab");
    }

    #[test]
    fn secrets_longest_first() {
        let s = Secrets::new(["abcdefgh", "abcdefgh-ijkl"]);
        assert_eq!(s.redact("x abcdefgh-ijkl y"), "x [REDACTED] y");
    }
}
