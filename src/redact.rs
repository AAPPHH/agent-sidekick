//! Removes secrets from text before it reaches logs, errors or tool responses.

pub const REDACTED: &str = "[REDACTED]";
const TOKEN_PREFIX: &str = "sk-ant-";

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
}
