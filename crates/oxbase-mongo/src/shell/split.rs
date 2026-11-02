//! Splitting shell scripts into statements (module docs, *Splitting*).
//!
//! A statement ends at a `;` outside brackets, or at a line break outside brackets where the
//! statement is complete: its last token is not an operator, `.`, `,` or `=>`, it is not the
//! head of a control statement (`if (...)`, `for (...)`, `while (...)`, `else`, a function
//! head, ...) waiting for its body, and the next significant token does not continue it (`.`,
//! `)`, `]`, `}`, `?`, `:`, a binary operator, `else`, `catch`, `finally`). A `;` followed by
//! `else` (or by the `while` of a `do` loop) does not end it. `use x`, `show x` and `it` lines
//! are statements of their own. A script whose first character after blanks and comments is
//! `{` is one request statement.

use std::ops::Range;

use oxbase_dialect::script::{statement, trim_range};
use oxbase_sql::{Statement, StatementKind};

use super::lang::statement_kind;
use super::lex::{Lexer, Tok};

/// Words that start a statement waiting for a body after its `( ... )` head.
const CONTROL_HEADS: [&str; 6] = ["if", "for", "while", "with", "switch", "catch"];
/// Words after which a body must follow on a later line.
const OPEN_ENDED: [&str; 5] = ["else", "do", "try", "finally", "function"];
/// Words that continue the statement before them.
const CONTINUATIONS: [&str; 3] = ["else", "catch", "finally"];

/// The shell commands written without parentheses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandWord {
    Use,
    Show,
    It,
}

/// `use x`, `show x` or `it` (the whole trimmed statement text, an optional `;` at the end):
/// the command and its argument.
pub(super) fn command_line(text: &str) -> Option<(CommandWord, &str)> {
    let text = text.trim();
    let text = text.strip_suffix(';').unwrap_or(text).trim_end();
    let mut words = text.split_whitespace();
    let first = words.next()?;
    let second = words.next();
    if words.next().is_some() {
        return None;
    }
    let word = match (first, second) {
        ("use", Some(_)) => CommandWord::Use,
        ("show", Some(_)) => CommandWord::Show,
        ("it", None) => CommandWord::It,
        _ => return None,
    };
    let argument = second.unwrap_or("");
    let plain = argument
        .bytes()
        .all(|b| !b"=(){}[],;'\"`+*/!<>?:".contains(&b));
    plain.then_some((word, argument))
}

/// Where a JSON request starts: the offset of the first `{` when only blanks and comments
/// (`//`, `/* */`) come before it.
pub(super) fn request_start(text: &str) -> Option<usize> {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        if let Some(after) = trimmed.strip_prefix("//") {
            rest = after.find('\n').map_or("", |i| &after[i..]);
        } else if let Some(after) = trimmed.strip_prefix("/*") {
            rest = &after[after.find("*/")? + 2..];
        } else {
            return trimmed.starts_with('{').then(|| text.len() - trimmed.len());
        }
    }
}

pub(super) fn split(text: &str) -> Vec<Statement> {
    if let Some(at) = request_start(text) {
        return trim_range(text, at..text.len())
            .map(|range| {
                let kind = statement_kind(&text[range.clone()]);
                vec![statement(text, range, text.len(), kind)]
            })
            .unwrap_or_default();
    }
    Splitter::new(text).run()
}

/// The state of one statement being read.
struct Current {
    start: usize,
    /// End of its last significant token.
    end: usize,
    last: Tok,
    last_range: Range<usize>,
    first_word: String,
    /// The last `(` at depth 0 opened a control head (`if (`, `function f(`).
    head_paren: bool,
    /// The last token closed a control head's `)`.
    after_head: bool,
}

struct Splitter<'a> {
    text: &'a str,
    lexer: Lexer<'a>,
    out: Vec<Statement>,
    current: Option<Current>,
    /// The two previous words of the statement (for `function name(`).
    words: [Option<Range<usize>>; 2],
}

impl<'a> Splitter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            lexer: Lexer::new(text),
            out: Vec::new(),
            current: None,
            words: [None, None],
        }
    }

    fn run(mut self) -> Vec<Statement> {
        while let Some((tok, range)) = self.lexer.next() {
            match tok {
                Tok::Newline => {
                    if self.current.is_some() && self.lexer.depth() == 0 && self.complete() {
                        self.finish(range.end);
                    }
                }
                Tok::Punct(b';') if self.lexer.depth() == 0 => {
                    if self.current.is_some() && !self.else_follows() {
                        self.finish(range.end);
                    } else if self.current.is_some() {
                        self.token(tok, range);
                    }
                }
                Tok::Word if self.current.is_none() && self.lexer.depth() == 0 => {
                    if !self.command(&range) {
                        self.token(tok, range);
                    }
                }
                _ => self.token(tok, range),
            }
        }
        if self.current.is_some() {
            self.finish(self.text.len());
        }
        self.out
    }

    /// A `use` / `show` / `it` line starting at `range`: pushed as a statement of its own.
    fn command(&mut self, range: &Range<usize>) -> bool {
        let rest = &self.text[range.start..];
        let line_len = rest.find('\n').unwrap_or(rest.len());
        let line = &rest[..line_len];
        let Some((word, _)) = command_line(line) else {
            return false;
        };
        let body = line.trim_end();
        let body = body.strip_suffix(';').unwrap_or(body).trim_end();
        let end = range.start + body.len();
        let segment_end = (range.start + line_len + 1).min(self.text.len());
        let kind = if word == CommandWord::Use {
            StatementKind::Other
        } else {
            StatementKind::Query
        };
        self.out
            .push(statement(self.text, range.start..end, segment_end, kind));
        self.lexer.restart_at(segment_end);
        true
    }

    fn token(&mut self, tok: Tok, range: Range<usize>) {
        let depth_after = self.lexer.depth();
        let text = self.text;
        let current = self.current.get_or_insert_with(|| Current {
            start: range.start,
            end: range.end,
            last: tok,
            last_range: range.clone(),
            first_word: if tok == Tok::Word {
                text[range.clone()].to_owned()
            } else {
                String::new()
            },
            head_paren: false,
            after_head: false,
        });
        if tok == Tok::Punct(b'(') && depth_after == 1 {
            let previous = self.words[1].clone().map(|r| &text[r]);
            let before = self.words[0].clone().map(|r| &text[r]);
            // The `while (...)` that ends a `do` loop is no head: nothing follows it.
            let do_while = previous == Some("while") && current.first_word == "do";
            current.head_paren = current.last == Tok::Word
                && !do_while
                && (previous.is_some_and(|w| CONTROL_HEADS.contains(&w))
                    || before == Some("function")
                    || previous == Some("function"));
        }
        current.after_head = tok == Tok::Punct(b')') && depth_after == 0 && current.head_paren;
        current.end = range.end;
        current.last = tok;
        current.last_range = range.clone();
        if tok == Tok::Word {
            self.words = [self.words[1].take(), Some(range)];
        }
    }

    /// Whether the next significant token is `else` (`if (x) a(); else b()`: the `;` ends the
    /// `if` branch, not the statement), or the `while` of a `do x(); while (y)`.
    fn else_follows(&self) -> bool {
        let do_loop = self.current.as_ref().is_some_and(|c| c.first_word == "do");
        let mut ahead = self.lexer.clone();
        ahead
            .find(|(t, _)| *t != Tok::Newline)
            .is_some_and(|(t, r)| {
                let word = &self.text[r];
                t == Tok::Word && (word == "else" || (do_loop && word == "while"))
            })
    }

    /// Whether the statement read so far ends at the line break just read.
    fn complete(&self) -> bool {
        let Some(current) = &self.current else {
            return false;
        };
        let last = &self.text[current.last_range.clone()];
        let ends_open = match current.last {
            Tok::Punct(_) => !matches!(last, ")" | "]" | "}" | "++" | "--"),
            Tok::Word => OPEN_ENDED.contains(&last),
            Tok::Template => !last.ends_with('`'),
            _ => false,
        };
        if ends_open || current.after_head {
            return false;
        }
        // The next significant token, read with the current state.
        let mut ahead = self.lexer.clone();
        let next = ahead.find(|(t, _)| *t != Tok::Newline);
        let Some((tok, range)) = next else {
            return true;
        };
        let followed_by_equals = self.text.as_bytes().get(range.end) == Some(&b'=');
        let next_text = &self.text[range];
        match tok {
            Tok::Punct(b'!') => !followed_by_equals,
            Tok::Punct(c) => !b".)]}?:,+-*/%&|^<>=".contains(&c),
            Tok::Word => {
                let continues = CONTINUATIONS.contains(&next_text)
                    || (next_text == "while" && current.first_word == "do");
                !continues
            }
            _ => true,
        }
    }

    fn finish(&mut self, segment_end: usize) {
        if let Some(current) = self.current.take() {
            let range = current.start..current.end;
            let kind = statement_kind(&self.text[range.clone()]);
            self.out
                .push(statement(self.text, range, segment_end, kind));
        }
        self.words = [None, None];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn texts(script: &str) -> Vec<String> {
        split(script).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn table_of_scripts() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("  \n // only a comment\n /* and another */ \n", &[]),
            ("db.c.find()", &["db.c.find()"]),
            ("db.a.find(); db.b.find();", &["db.a.find()", "db.b.find()"]),
            (
                "db.a.find()\ndb.b.find()\n",
                &["db.a.find()", "db.b.find()"],
            ),
            (
                "db.c.find({a: 1})\n  .sort({b: -1})\n  .limit(5)\n\nprint(1)",
                &[
                    "db.c.find({a: 1})\n  .sort({b: -1})\n  .limit(5)",
                    "print(1)",
                ],
            ),
            (
                "db.c.find({\n  a: 1,\n  b: [1,\n 2]\n})",
                &["db.c.find({\n  a: 1,\n  b: [1,\n 2]\n})"],
            ),
            ("var x = 1 +\n  2\nx", &["var x = 1 +\n  2", "x"]),
            ("var x = 1\n  + 2", &["var x = 1\n  + 2"]),
            ("a = b ?\n c :\n d", &["a = b ?\n c :\n d"]),
            ("x++\ny--\n", &["x++", "y--"]),
            ("f(x =>\n x + 1)", &["f(x =>\n x + 1)"]),
            (
                "const f = x =>\n  x * 2\nf(2)",
                &["const f = x =>\n  x * 2", "f(2)"],
            ),
            (
                "print('a; b // c')\nprint(\"d\\\"; e\")",
                &["print('a; b // c')", "print(\"d\\\"; e\")"],
            ),
            (
                "print(`a\n${ 1 + \n 2 }\nb`)\nx",
                &["print(`a\n${ 1 + \n 2 }\nb`)", "x"],
            ),
            ("/* lead */ db.c.find() // trail\n", &["db.c.find()"]),
            ("// a\n// b\ndb.c.find()", &["db.c.find()"]),
            (
                "db.c.find({a: /x;y\\/z/})\nz",
                &["db.c.find({a: /x;y\\/z/})", "z"],
            ),
            (
                "use oxbase_test\nshow collections\nit",
                &["use oxbase_test", "show collections", "it"],
            ),
            (
                "use oxbase_test;\ndb.c.find()",
                &["use oxbase_test", "db.c.find()"],
            ),
            ("  show dbs  \n", &["show dbs"]),
            ("use = 3\nuser.x", &["use = 3", "user.x"]),
            (
                "if (x)\n  print(1)\nelse\n  print(2)\nprint(3)",
                &["if (x)\n  print(1)\nelse\n  print(2)", "print(3)"],
            ),
            (
                "for (let i = 0; i < 3; i++)\n  print(i)\n",
                &["for (let i = 0; i < 3; i++)\n  print(i)"],
            ),
            (
                "function f(a)\n{\n  return a\n}\nf(1)",
                &["function f(a)\n{\n  return a\n}", "f(1)"],
            ),
            (
                "try {\n  x()\n}\ncatch (e) {\n  print(e)\n}\nfinally {\n}\ny",
                &[
                    "try {\n  x()\n}\ncatch (e) {\n  print(e)\n}\nfinally {\n}",
                    "y",
                ],
            ),
            ("print(1);;  ;print(2)", &["print(1)", "print(2)"]),
            ("db.c.find(\n", &["db.c.find("]),
            ("print('open\nprint(2)", &["print('open\nprint(2)"]),
            ("x = 'open\nprint(2)", &["x = 'open", "print(2)"]),
            ("x = 1 // ok\n", &["x = 1"]),
            ("a\n!b", &["a", "!b"]),
            ("a\n!== b", &["a\n!== b"]),
            (
                "if (x) a(); else b()\nc()",
                &["if (x) a(); else b()", "c()"],
            ),
            (
                "if (x) a();\nelse b(); c()",
                &["if (x) a();\nelse b()", "c()"],
            ),
            ("x = 1; elsewhere()", &["x = 1", "elsewhere()"]),
            ("do x(); while (y)\nz", &["do x(); while (y)", "z"]),
            (
                "do {\n  x()\n} while (y)\nz",
                &["do {\n  x()\n} while (y)", "z"],
            ),
        ];
        for (script, want) in cases {
            assert_eq!(texts(script), *want, "{script:?}");
        }
    }

    #[test]
    fn a_json_request_is_one_statement() {
        let script = "  {\"type\": \"find\",\n \"collection\": {\"kind\": \"table\", \"catalog\": \"d\", \"name\": \"c\"}}\n\n";
        let s = split(script);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text, script.trim());
        assert_eq!(s[0].kind, StatementKind::Query);
        assert_eq!(s[0].segment_end, script.len());

        // Comments before the request are not part of it.
        let script = "// q\n/* more */ {\"type\": \"find\"}\n";
        let s = split(script);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].text, "{\"type\": \"find\"}");
        assert_eq!(s[0].kind, StatementKind::Query);
        assert_eq!(request_start(script), Some(16));
        assert_eq!(request_start("/* open {"), None);
        assert_eq!(request_start("// {\nx"), None);
        assert_eq!(request_start("x {"), None);
    }

    #[test]
    fn ranges_positions_and_segments() {
        let script = "db.a.find();  // x\n\n  db.b.findOne({ü: 1})\nuse x\n";
        let s = split(script);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].range, 0..11);
        assert_eq!(s[0].segment_end, 12);
        assert_eq!(&script[s[1].range.clone()], "db.b.findOne({ü: 1})");
        assert_eq!((s[1].start.line, s[1].start.column), (3, 3));
        assert_eq!(s[1].end.column, 23);
        assert_eq!(s[2].text, "use x");
        assert_eq!(s[2].segment_end, script.len());
        assert_eq!(s[0].kind, StatementKind::Query);
        assert_eq!(s[2].kind, StatementKind::Other);
    }

    #[test]
    fn command_lines() {
        assert_eq!(
            command_line("use oxbase_test"),
            Some((CommandWord::Use, "oxbase_test"))
        );
        assert_eq!(
            command_line(" show  dbs ; "),
            Some((CommandWord::Show, "dbs"))
        );
        assert_eq!(command_line("it"), Some((CommandWord::It, "")));
        for text in [
            "use", "use a b", "use(x)", "use = 1", "show", "it = 2", "user x", "show 'x'",
        ] {
            assert_eq!(command_line(text), None, "{text}");
        }
    }
}
