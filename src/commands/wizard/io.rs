//! The wizard's dialogue with the user, behind [`Prompter`] so tests can script it.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};

use crate::token::Token;

/// Questions and output of the wizard. Every answer is validated by the implementation's
/// caller where needed; an `Err` ends the wizard (e.g. end of input).
pub trait Prompter {
    /// Print one block of text (a line break is added).
    fn say(&mut self, text: &str);
    /// Yes/no question; `default` is the answer on an empty line.
    fn confirm(&mut self, question: &str, default: bool) -> Result<bool>;
    /// Pick exactly one of `options` (indices); `default` on an empty line.
    fn choose_one(&mut self, question: &str, options: &[String], default: usize) -> Result<usize>;
    /// Pick any number of `options` (indices, ascending, without duplicates); `defaults` on
    /// an empty line. May return an empty list; the caller decides whether that is allowed.
    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        defaults: &[usize],
    ) -> Result<Vec<usize>>;
    /// Read a secret without echo.
    fn secret(&mut self, prompt: &str) -> Result<Token>;
}

/// Parse a yes/no answer (German and English). `None`: not understood.
pub fn parse_yes_no(line: &str, default: bool) -> Option<bool> {
    match line.trim().to_lowercase().as_str() {
        "" => Some(default),
        "j" | "ja" | "y" | "yes" => Some(true),
        "n" | "nein" | "no" => Some(false),
        _ => None,
    }
}

/// Parse a selection like `1`, `1,3` or `1 3` (1-based) into sorted, unique 0-based
/// indices below `count`. Empty input: `defaults`. `None`: invalid.
pub fn parse_selection(line: &str, count: usize, defaults: &[usize]) -> Option<Vec<usize>> {
    let line = line.trim();
    if line.is_empty() {
        return Some(defaults.to_vec());
    }
    let mut picked = Vec::new();
    for part in line.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        if part.is_empty() {
            continue;
        }
        let n: usize = part.parse().ok()?;
        if n == 0 || n > count {
            return None;
        }
        picked.push(n - 1);
    }
    picked.sort_unstable();
    picked.dedup();
    Some(picked)
}

/// [`Prompter`] on the real terminal: questions on stdout, answers as lines from stdin,
/// secrets through `rpassword` (no echo).
pub struct Terminal;

impl Terminal {
    fn read_line(&mut self, prompt: &str) -> Result<String> {
        print!("{prompt}");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let n = std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("reading the answer from the terminal")?;
        if n == 0 {
            bail!(
                "Eingabe beendet (Strg+D/Strg+Z). Nichts weiter geändert; starte \
                 `agent-sidekick setup` erneut, wenn du fortfahren willst."
            );
        }
        Ok(line)
    }
}

/// How often an answer that is not understood is asked again.
const MAX_TRIES: usize = 5;

impl Prompter for Terminal {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "[J/n]" } else { "[j/N]" };
        for _ in 0..MAX_TRIES {
            let line = self.read_line(&format!("{question} {hint} "))?;
            match parse_yes_no(&line, default) {
                Some(v) => return Ok(v),
                None => println!("Bitte mit j (ja) oder n (nein) antworten."),
            }
        }
        bail!("keine gültige Antwort; starte `agent-sidekick setup` erneut")
    }

    fn choose_one(&mut self, question: &str, options: &[String], default: usize) -> Result<usize> {
        println!("{question}");
        for (i, o) in options.iter().enumerate() {
            let mark = if i == default { " (Vorschlag)" } else { "" };
            println!("  {}) {o}{mark}", i + 1);
        }
        for _ in 0..MAX_TRIES {
            let line = self.read_line(&format!("Nummer [{}]: ", default + 1))?;
            match parse_selection(&line, options.len(), &[default]) {
                Some(v) if v.len() == 1 => return Ok(v[0]),
                _ => println!(
                    "Bitte genau eine Nummer von 1 bis {} eingeben.",
                    options.len()
                ),
            }
        }
        bail!("keine gültige Auswahl; starte `agent-sidekick setup` erneut")
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        defaults: &[usize],
    ) -> Result<Vec<usize>> {
        println!("{question}");
        for (i, o) in options.iter().enumerate() {
            let mark = if defaults.contains(&i) { "[x]" } else { "[ ]" };
            println!("  {}) {mark} {o}", i + 1);
        }
        let default_text: Vec<String> = defaults.iter().map(|d| (d + 1).to_string()).collect();
        for _ in 0..MAX_TRIES {
            let line = self.read_line(&format!(
                "Nummern mit Komma getrennt, z. B. 1,3 (Enter: {}): ",
                if default_text.is_empty() {
                    "keine".to_owned()
                } else {
                    default_text.join(",")
                }
            ))?;
            match parse_selection(&line, options.len(), defaults) {
                Some(v) => return Ok(v),
                None => println!("Nur Nummern von 1 bis {} angeben.", options.len()),
            }
        }
        bail!("keine gültige Auswahl; starte `agent-sidekick setup` erneut")
    }

    fn secret(&mut self, prompt: &str) -> Result<Token> {
        let raw = rpassword::prompt_password(prompt)
            .context("reading the secret from the terminal (input hidden)")?;
        Ok(Token::new(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_no() {
        assert_eq!(parse_yes_no("", true), Some(true));
        assert_eq!(parse_yes_no(" \n", false), Some(false));
        assert_eq!(parse_yes_no("J", false), Some(true));
        assert_eq!(parse_yes_no("nein", true), Some(false));
        assert_eq!(parse_yes_no("y", false), Some(true));
        assert_eq!(parse_yes_no("vielleicht", true), None);
    }

    #[test]
    fn selection() {
        assert_eq!(parse_selection("", 3, &[0]), Some(vec![0]));
        assert_eq!(parse_selection("3,1 1", 3, &[0]), Some(vec![0, 2]));
        assert_eq!(parse_selection("2;3\n", 3, &[]), Some(vec![1, 2]));
        assert_eq!(parse_selection("0", 3, &[]), None);
        assert_eq!(parse_selection("4", 3, &[]), None);
        assert_eq!(parse_selection("x", 3, &[]), None);
        assert_eq!(parse_selection(",", 3, &[1]), Some(vec![]));
    }
}
