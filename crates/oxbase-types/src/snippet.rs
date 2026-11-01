//! M6: SQL snippets (`docs/m6-contracts.md` §6).
//!
//! A snippet expands a short `prefix` typed in the editor into a `body` with tab stops. The body
//! uses the snippet syntax Monaco understands (a subset of the TextMate syntax):
//!
//! - `$1`, `$2`, ... tab stops and `$0` the final cursor position;
//! - `${1:default text}` placeholders (they may nest: `${1:a ${2:b}}`);
//! - `${1|one,two,three|}` choices;
//! - `$name` / `${name}` / `${name:default}` variables (Monaco's, such as `$TM_SELECTED_TEXT`);
//! - `\$`, `\}` and `\\` escape those characters; inside a choice `\,` and `\|` too.
//!
//! A `$` that starts none of these is plain text (so a PostgreSQL `$$` body quote may be
//! written as is). Built-in snippets live in `oxbase-insight`; user snippets in the app
//! database (`oxbase_store::SnippetRepository`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Engine, Error, Result};

/// A snippet as the editor sees it (built-in or user-defined).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    /// `builtin:<slug>` for built-in snippets, the UUID text for user snippets.
    pub id: String,
    pub name: String,
    /// What the user types to get it offered.
    pub prefix: String,
    /// The body in the syntax of the module docs.
    pub body: String,
    #[serde(default)]
    pub description: String,
    /// Engines the snippet is offered on; empty = every SQL engine.
    #[serde(default)]
    pub engines: Vec<Engine>,
    /// Built-in snippets cannot be changed or deleted.
    #[serde(default)]
    pub builtin: bool,
}

impl Snippet {
    /// `true` when the snippet is offered on `engine`.
    pub fn applies_to(&self, engine: Engine) -> bool {
        self.engines.is_empty() || self.engines.contains(&engine)
    }
}

/// A user snippet to create (`id: None`) or replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnippetInput {
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    pub prefix: String,
    pub body: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub engines: Vec<Engine>,
}

impl SnippetInput {
    pub const MAX_NAME_CHARS: usize = 100;
    pub const MAX_PREFIX_CHARS: usize = 40;
    pub const MAX_BODY_CHARS: usize = 10_000;
    pub const MAX_DESCRIPTION_CHARS: usize = 500;

    /// Checks the input: a name of 1 to 100 characters; a prefix of 1 to 40 characters from
    /// `A-Z a-z 0-9 _ -` (what the completion word pattern can match); a body of 1 to 10 000
    /// characters that [`validate_snippet_body`] accepts; a description of at most 500
    /// characters; no MongoDB or Redis engine.
    pub fn validate(&self) -> Result<()> {
        let chars = |s: &str| s.chars().count();
        if self.name.trim().is_empty() || chars(&self.name) > Self::MAX_NAME_CHARS {
            return Err(Error::invalid_input(format!(
                "a snippet name needs 1 to {} characters",
                Self::MAX_NAME_CHARS
            )));
        }
        let prefix_ok = !self.prefix.is_empty()
            && chars(&self.prefix) <= Self::MAX_PREFIX_CHARS
            && self
                .prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !prefix_ok {
            return Err(Error::invalid_input(format!(
                "a snippet prefix needs 1 to {} characters from A-Z a-z 0-9 _ -",
                Self::MAX_PREFIX_CHARS
            )));
        }
        if self.body.trim().is_empty() || chars(&self.body) > Self::MAX_BODY_CHARS {
            return Err(Error::invalid_input(format!(
                "a snippet body needs 1 to {} characters",
                Self::MAX_BODY_CHARS
            )));
        }
        if chars(&self.description) > Self::MAX_DESCRIPTION_CHARS {
            return Err(Error::invalid_input(format!(
                "a snippet description has at most {} characters",
                Self::MAX_DESCRIPTION_CHARS
            )));
        }
        if let Some(e) = self
            .engines
            .iter()
            .find(|e| matches!(e, Engine::MongoDb | Engine::Redis))
        {
            return Err(Error::invalid_input(format!(
                "SQL snippets are not offered on {}",
                e.display_name()
            )));
        }
        validate_snippet_body(&self.body)
    }
}

/// Checks that `body` is well-formed snippet syntax (module docs): every `${` is closed, tab
/// stop numbers are at most 999, choices end with `|}`.
pub fn validate_snippet_body(body: &str) -> Result<()> {
    let chars: Vec<char> = body.chars().collect();
    let mut parser = BodyParser {
        chars: &chars,
        at: 0,
    };
    parser.text(false)?;
    if parser.at < chars.len() {
        return Err(parser.error("an unmatched `}`; write `\\}` for a literal brace"));
    }
    Ok(())
}

struct BodyParser<'a> {
    chars: &'a [char],
    at: usize,
}

impl BodyParser<'_> {
    const MAX_TABSTOP: u32 = 999;

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn error(&self, what: &str) -> Error {
        Error::invalid_input(format!(
            "snippet body: {what} (at character {})",
            self.at + 1
        ))
    }

    /// Text up to the end, or (inside a placeholder) up to its closing `}` (not consumed).
    fn text(&mut self, nested: bool) -> Result<()> {
        while let Some(c) = self.peek() {
            match c {
                '\\' => self.at += 2,
                // Inside a placeholder: its end. At top level: an unmatched brace, which
                // `validate_snippet_body` reports.
                '}' => return Ok(()),
                '$' => self.dollar()?,
                _ => self.at += 1,
            }
        }
        if nested {
            return Err(self.error("a `${` without its closing `}`"));
        }
        Ok(())
    }

    fn digits(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        (self.at > start).then(|| {
            self.chars[start..self.at]
                .iter()
                .collect::<String>()
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        })
    }

    fn name(&mut self) -> bool {
        if !self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return false;
        }
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.at += 1;
        }
        true
    }

    fn tabstop(&mut self) -> Result<Option<u32>> {
        match self.digits() {
            Some(n) if n > Self::MAX_TABSTOP => Err(self.error("a tab stop number above 999")),
            other => Ok(other),
        }
    }

    /// At a `$`.
    fn dollar(&mut self) -> Result<()> {
        self.at += 1;
        if self.peek() != Some('{') {
            // `$1`, `$name`, or a plain `$`.
            if self.tabstop()?.is_none() {
                self.name();
            }
            return Ok(());
        }
        self.at += 1;
        let is_tabstop = self.tabstop()?.is_some();
        if !is_tabstop && !self.name() {
            return Err(self.error("`${` must be followed by a tab stop number or a name"));
        }
        match self.peek() {
            Some('}') => {
                self.at += 1;
                Ok(())
            }
            Some(':') => {
                self.at += 1;
                self.text(true)?;
                // `text(true)` stops only at the closing brace.
                self.at += 1;
                Ok(())
            }
            Some('|') if is_tabstop => self.choice(),
            _ => Err(self.error("expected `}`, `:` or `|` after the tab stop")),
        }
    }

    /// After `${n` at the opening `|`.
    fn choice(&mut self) -> Result<()> {
        self.at += 1;
        while let Some(c) = self.peek() {
            match c {
                '\\' => self.at += 2,
                '|' if self.chars.get(self.at + 1) == Some(&'}') => {
                    self.at += 2;
                    return Ok(());
                }
                _ => self.at += 1,
            }
        }
        Err(self.error("a choice without its closing `|}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prefix: &str, body: &str) -> SnippetInput {
        SnippetInput {
            id: None,
            name: "Select".into(),
            prefix: prefix.into(),
            body: body.into(),
            description: String::new(),
            engines: vec![],
        }
    }

    #[test]
    fn valid_bodies() {
        for body in [
            "SELECT $1 FROM $2;$0",
            "SELECT ${1:*} FROM ${2:table} WHERE ${3:a ${4:= 1}}",
            "ORDER BY ${1|ASC,DESC|}",
            "${TM_SELECTED_TEXT} $CLIPBOARD ${name:x}",
            "AS $$ BEGIN END $$ \\$1 \\} literal",
            "price $ 5",
        ] {
            validate_snippet_body(body).unwrap_or_else(|e| panic!("{body}: {e}"));
        }
    }

    #[test]
    fn invalid_bodies() {
        for body in [
            "SELECT ${1:x",
            "${",
            "${:x}",
            "${1|a,b}",
            "${1000}",
            "a } b",
            "${1 x}",
        ] {
            assert!(validate_snippet_body(body).is_err(), "{body}");
        }
    }

    #[test]
    fn input_validation() {
        input("sel", "SELECT 1").validate().unwrap();
        assert!(input("", "SELECT 1").validate().is_err());
        assert!(input("se l", "SELECT 1").validate().is_err());
        assert!(input("sel", "  ").validate().is_err());
        let mut bad = input("sel", "SELECT 1");
        bad.engines = vec![Engine::Redis];
        assert!(bad.validate().is_err());
        bad.engines = vec![Engine::Postgres];
        bad.validate().unwrap();
        bad.name = "x".repeat(101);
        assert!(bad.validate().is_err());
    }

    #[test]
    fn engine_filter_and_serde() {
        let s = Snippet {
            id: "builtin:sel".into(),
            name: "Select".into(),
            prefix: "sel".into(),
            body: "SELECT $0".into(),
            description: String::new(),
            engines: vec![Engine::MsSql],
            builtin: true,
        };
        assert!(s.applies_to(Engine::MsSql));
        assert!(!s.applies_to(Engine::Postgres));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["engines"], serde_json::json!(["mssql"]));
        assert_eq!(json["builtin"], serde_json::json!(true));
        let input: SnippetInput = serde_json::from_value(serde_json::json!({
            "name": "n", "prefix": "p", "body": "b"
        }))
        .unwrap();
        assert_eq!(input.id, None);
        assert!(input.engines.is_empty());
    }
}
