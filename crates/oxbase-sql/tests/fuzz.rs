//! Property tests with a small deterministic generator (no extra dependency):
//!
//! - random scripts built from syntax fragments and random characters never panic and always
//!   produce well-formed statements (ordered, non-overlapping, exact text, trimmed, positions
//!   equal to `position_at`); `statement_at` and `redact_secrets` never panic either;
//! - scripts of plain words, whitespace and `;` split exactly like a naive `;` split;
//! - joining well-formed statements (with quotes, comments and bodies that hide `;`) with
//!   terminators gives exactly those statements back.

use std::borrow::Cow;

use oxbase_sql::{
    SqlDialect, Statement, pick_statement, position_at, redact_secrets, split, statement_at,
};
use pretty_assertions::assert_eq;

const PG: SqlDialect = SqlDialect::Postgres;
const MYSQL: SqlDialect = SqlDialect::MySql { mariadb: false };
const MARIADB: SqlDialect = SqlDialect::MySql { mariadb: true };
const SQLITE: SqlDialect = SqlDialect::Sqlite;
const TSQL: SqlDialect = SqlDialect::TSql;
const ALL: [SqlDialect; 5] = [PG, MYSQL, MARIADB, SQLITE, TSQL];

/// xorshift64: deterministic, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("small");
        usize::try_from(self.next() % n).expect("below n")
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_space_char(c: char) -> bool {
    u8::try_from(c).is_ok_and(is_space)
}

fn trim(s: &str) -> &str {
    s.trim_matches(is_space_char)
}

/// Checks every invariant of a split result; returns the statements.
#[track_caller]
fn check(dialect: SqlDialect, script: &str, rng: &mut Rng) -> Vec<Statement> {
    let statements = split(dialect, script);
    let mut previous_segment_end = 0;
    for s in &statements {
        let context = format!("{dialect:?} {script:?} {s:?}");
        assert!(s.range.start >= previous_segment_end, "order: {context}");
        assert!(s.range.start < s.range.end, "empty: {context}");
        assert!(s.range.end <= s.segment_end, "segment: {context}");
        assert!(s.segment_end <= script.len(), "segment: {context}");
        assert_eq!(
            script.get(s.range.clone()),
            Some(s.text.as_str()),
            "text: {context}"
        );
        let bytes = s.text.as_bytes();
        assert!(!is_space(bytes[0]), "leading space: {context}");
        // Only an unterminated construct running to the end may end in whitespace.
        assert!(
            !is_space(bytes[bytes.len() - 1]) || s.range.end == script.len(),
            "trailing space: {context}"
        );
        assert_eq!(s.start, position_at(script, s.range.start), "{context}");
        assert_eq!(s.end, position_at(script, s.range.end), "{context}");
        previous_segment_end = s.segment_end;
    }
    let offsets = [
        0,
        script.len(),
        script.len() + 3,
        rng.below(script.len() + 1),
        rng.below(script.len() + 1),
    ];
    for offset in offsets {
        assert_eq!(
            statement_at(dialect, script, offset),
            pick_statement(&statements, offset).cloned()
        );
    }
    if let Cow::Borrowed(same) = redact_secrets(dialect, script) {
        assert_eq!(same, script);
    }
    statements
}

const FRAGMENTS: &[&str] = &[
    "SELECT",
    "select",
    "x",
    "1",
    " ",
    "  ",
    "\n",
    "\r\n",
    "\t",
    "\u{b}",
    ";",
    ";;",
    "'",
    "''",
    "\"",
    "`",
    "[",
    "]",
    "]]",
    "$",
    "$$",
    "$a$",
    "$1",
    "E'",
    "e'",
    "N'",
    "\\",
    "\\'",
    "--",
    "-- c",
    "--1",
    "#",
    "/*",
    "*/",
    "/*!",
    "/*!5 ",
    "/*M!",
    "/*+",
    "/* /*",
    "(",
    ")",
    ":",
    ":=",
    "=",
    "@",
    ".",
    ",",
    "&",
    "BEGIN",
    "END",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "ELSEIF",
    "IF",
    "LOOP",
    "WHILE",
    "REPEAT",
    "FOR",
    "DO",
    "ATOMIC",
    "NOT",
    "CREATE",
    "OR",
    "REPLACE",
    "TEMP",
    "TRIGGER",
    "PROCEDURE",
    "FUNCTION",
    "EVENT",
    "DEFINER",
    "ROW",
    "EACH",
    "lbl:",
    "DELIMITER",
    "\nDELIMITER $$\n",
    "\nDELIMITER //\n",
    "\nDELIMITER ;\n",
    "\ndelimiter END\n",
    "\nDELIMITER\n",
    "//",
    "\nGO\n",
    "GO",
    "\ngo 3 -- x\n",
    "PASSWORD",
    "IDENTIFIED",
    "BY",
    "WITH",
    "KEY",
    "SECRET",
    "password=",
    "pwd = ",
    "_utf8",
    "é",
    "😀",
    "ß",
    "日本",
    "\u{0}",
    "\u{7f}",
    "\u{a0}",
    "\u{2028}",
    "\u{feff}",
];

fn random_script(rng: &mut Rng) -> String {
    let mut script = String::new();
    for _ in 0..rng.below(48) {
        if rng.below(10) == 0 {
            // Any scalar value, including controls and astral characters.
            let raw = u32::try_from(rng.below(0x11_0000)).expect("fits");
            script.push(char::from_u32(raw).unwrap_or('\u{fffd}'));
        } else {
            script.push_str(rng.pick(FRAGMENTS));
        }
    }
    script
}

#[test]
fn random_scripts_never_panic_and_stay_well_formed() {
    for seed in 0..3000 {
        let mut rng = Rng::new(seed);
        let script = random_script(&mut rng);
        for dialect in ALL {
            check(dialect, &script, &mut rng);
        }
    }
}

#[test]
fn adversarial_inputs() {
    let mut rng = Rng::new(7);
    let inputs = [
        "$".repeat(1000),
        "$a".repeat(500),
        "/*".repeat(1000),
        "*/".repeat(1000),
        "'".repeat(1001),
        "\\".repeat(1001),
        "-".repeat(1001),
        "BEGIN ".repeat(500),
        "END ".repeat(500),
        "CASE ".repeat(500),
        format!("CREATE TRIGGER t {}", "BEGIN ;".repeat(300)),
        format!("CREATE PROCEDURE p() {}", "BEGIN IF x THEN ".repeat(300)),
        format!("CREATE PROCEDURE p() BEGIN {}", "END ".repeat(300)),
        "DELIMITER ".repeat(300),
        "\nDELIMITER a\n".repeat(100) + "aaa",
        "\nGO".repeat(300),
        "😀".repeat(300),
    ];
    for input in &inputs {
        for dialect in ALL {
            check(dialect, input, &mut rng);
        }
    }
}

#[test]
fn plain_words_split_like_a_naive_semicolon_split() {
    const WORDS: &[&str] = &[
        "a", "SELECT", "x1", "é", "😀z", "42", "(", ")", ",", "+", " ", "\n", "\t", "\r\n", ";",
    ];
    for seed in 0..2000 {
        let mut rng = Rng::new(seed);
        let mut script = String::new();
        for _ in 0..rng.below(40) {
            script.push_str(rng.pick(WORDS));
        }
        // Expected: every `;`-separated segment, trimmed, non-empty, with exact ranges.
        let mut expected = Vec::new();
        let mut segment_start = 0;
        let terminators = script.match_indices(';').map(|(at, _)| at);
        for (at, terminated) in terminators
            .map(|at| (at, true))
            .chain([(script.len(), false)])
        {
            let segment = &script[segment_start..at];
            let text = trim(segment);
            if !text.is_empty() {
                let leading = segment.len() - segment.trim_start_matches(is_space_char).len();
                let start = segment_start + leading;
                let end = start + text.len();
                let segment_end = if terminated { at + 1 } else { end };
                expected.push((start..end, segment_end, text.to_owned()));
            }
            segment_start = at + 1;
        }
        for dialect in [PG, MYSQL, MARIADB, SQLITE] {
            let got: Vec<_> = check(dialect, &script, &mut rng)
                .into_iter()
                .map(|s| (s.range, s.segment_end, s.text))
                .collect();
            assert_eq!(got, expected, "{dialect:?} seed {seed}: {script:?}");
        }
    }
}

/// Self-contained pieces of statement text in which no `;` (or `GO` line) ends a statement.
fn atoms(dialect: SqlDialect) -> &'static [&'static str] {
    match dialect {
        SqlDialect::Postgres => &[
            "x",
            "1",
            "'a;b'",
            "'it''s;'",
            "\"a;\"\"b\"",
            "$$a;b$$",
            "$q$ $$ ; $q$",
            r"E'x\';'",
            "/* a /* ; */ */",
            "-- ;x\n",
            "U&'d;'",
            "a$b",
            "$1",
            "x::int",
            "é😀",
            "(",
            ")",
            "=",
            "`",
            "#",
        ],
        SqlDialect::MySql { .. } => &[
            "x",
            "1",
            r"'a\';'",
            "'it''s;'",
            "\"b;\\\"\"",
            "`c;``d`",
            "# ;\n",
            "-- ;\n",
            "/*!40101 ; */",
            "/*+ ; */",
            "/*M!1 ; */",
            "/* ; */",
            "--1",
            "@a := 1",
            "x'41'",
            "é😀",
            "(",
            ")",
            "$x",
        ],
        SqlDialect::Sqlite => &[
            "x", "1", "'a;''b'", "\"c;\"", "`d;`", "[e;f]", "/* ; */", "-- ;\n", "é😀", "?1",
            ":name", "(", ")", "#", r"'\'",
        ],
        SqlDialect::TSql => &[
            "x",
            "1",
            "'a;b'",
            "[x;y]]z]",
            "N'é'",
            "/* /* GO */ */",
            "-- c\n",
            ";",
            "\"q\"",
            "@v",
            "#t",
            "GOTO",
            "(",
        ],
    }
}

/// Whole statements with compound bodies.
fn compound_statements(dialect: SqlDialect) -> &'static [&'static str] {
    match dialect {
        SqlDialect::Postgres => &[
            "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT CASE WHEN true THEN 1 END; END",
            "DO $$ BEGIN PERFORM 1; END $$",
            "BEGIN",
            "END",
        ],
        SqlDialect::MySql { mariadb: false } => &[
            "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; END",
            "CREATE DEFINER=`u`@`%` TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET NEW.a = CASE WHEN 1 THEN 2 END; END",
            "BEGIN",
        ],
        SqlDialect::MySql { mariadb: true } => &[
            "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; END",
            "BEGIN NOT ATOMIC SELECT 1; END",
            "WHILE 1 DO SELECT 1; END WHILE",
            "BEGIN",
        ],
        SqlDialect::Sqlite => &[
            "CREATE TRIGGER t AFTER INSERT ON x BEGIN SELECT CASE WHEN 1 THEN 2 END; DELETE FROM y; END",
            "BEGIN",
            "END",
        ],
        SqlDialect::TSql => &["BEGIN SELECT 1; END"],
    }
}

/// A statement as written in the script (it may end with the line feed of a line comment).
fn random_statement(dialect: SqlDialect, rng: &mut Rng) -> String {
    if rng.below(5) == 0 {
        return rng.pick(compound_statements(dialect)).to_owned();
    }
    let mut parts = vec![rng.pick(&["SELECT", "x", "UPDATE", "é", "INSERT"])];
    for _ in 0..rng.below(7) {
        parts.push(rng.pick(atoms(dialect)));
    }
    parts.join(" ")
}

#[test]
fn joined_statements_split_back() {
    const SEPARATORS: &[&str] = &[
        ";",
        ";\n",
        " ;\n\n",
        ";\n-- c;\n",
        "; /* c; */ ",
        ";;\n",
        "\n;\n",
    ];
    const GO_SEPARATORS: &[&str] = &["\nGO\n", "\ngo 2\n", "\nGO -- next\n", "\n  GO\n"];
    for seed in 0..1500 {
        let mut rng = Rng::new(seed);
        for dialect in ALL {
            let count = 1 + rng.below(6);
            let statements: Vec<String> = (0..count)
                .map(|_| random_statement(dialect, &mut rng))
                .collect();
            let expected: Vec<&str> = statements.iter().map(String::as_str).map(trim).collect();
            let separators = if dialect == TSQL {
                GO_SEPARATORS
            } else {
                SEPARATORS
            };
            let mut script = String::new();
            for (i, statement) in statements.iter().enumerate() {
                script.push_str(statement);
                if i + 1 < count || rng.below(2) == 0 {
                    script.push_str(rng.pick(separators));
                }
            }
            let texts: Vec<String> = check(dialect, &script, &mut rng)
                .into_iter()
                .map(|s| s.text)
                .collect();
            assert_eq!(texts, expected, "{dialect:?} seed {seed}: {script:?}");

            // The same statements under a MySQL client delimiter.
            if matches!(dialect, SqlDialect::MySql { .. }) {
                let mut script = String::from("DELIMITER //\n");
                for statement in &statements {
                    script.push_str(statement);
                    script.push_str(rng.pick(&["//", "//\n", " //\n\n"]));
                }
                script.push_str("\nDELIMITER ;\n");
                let texts: Vec<String> = check(dialect, &script, &mut rng)
                    .into_iter()
                    .map(|s| s.text)
                    .collect();
                assert_eq!(texts, expected, "{dialect:?} seed {seed}: {script:?}");
            }
        }
    }
}

/// Statements holding one secret, written `{s}`; each is a single statement in its dialect.
fn secret_templates(dialect: SqlDialect) -> Vec<&'static str> {
    let mut templates = vec![
        "CREATE USER u IDENTIFIED BY '{s}'",
        "SET PASSWORD = '{s}'",
        "UPDATE t SET password = /* c */ '{s}'",
        "SELECT dblink_connect('host=h password={s} port=1')",
        "SELECT 'Server=x;Pwd={s};'",
    ];
    templates.extend_from_slice(match dialect {
        SqlDialect::Postgres => &[
            "ALTER ROLE r PASSWORD '{s}'",
            "ALTER ROLE r PASSWORD $q${s}$q$",
            "ALTER ROLE r PASSWORD E'{s}'",
            "ALTER ROLE r PASSWORD U&'{s}'",
            "DO $$ BEGIN EXECUTE 'ALTER ROLE r PASSWORD ''{s}'''; END $$",
            "CREATE USER MAPPING FOR u SERVER s OPTIONS (password '{s}')",
        ][..],
        SqlDialect::MySql { mariadb } => {
            if mariadb {
                &[
                    "CREATE USER u IDENTIFIED VIA ed25519 USING '{s}'",
                    "SET PASSWORD FOR 'u'@'%' = '{s}'",
                ][..]
            } else {
                &[
                    "CREATE USER u IDENTIFIED WITH p BY \"{s}\"",
                    "SET PASSWORD FOR 'u'@'%' = '{s}'",
                    "CHANGE MASTER TO MASTER_PASSWORD = '{s}'",
                    "SELECT 'host=x password=\\'{s} b\\''",
                    "/*!80001 CREATE USER u IDENTIFIED BY '{s}' */",
                    "PREPARE p FROM 'ALTER USER u IDENTIFIED BY \\'{s}\\''",
                ][..]
            }
        }
        SqlDialect::Sqlite => &[
            "PRAGMA key = '{s}'",
            "PRAGMA key = {s}",
            "ATTACH 'f.db' AS a KEY '{s}'",
            "ATTACH 'file:f.db?key={s}' AS a",
        ][..],
        SqlDialect::TSql => &["ALTER LOGIN l WITH PASSWORD = N'{s}'"][..],
    });
    templates
}

/// A statement without secrets: redaction must keep it byte for byte.
fn benign_statement(rng: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "x", "1", "'abc'", "'a;b'", "'it''s'", "(", ")", ",", "+", "=", "é😀", "/* c */", "-- c\n",
        "t.end", "WHERE", "name", "key", "FROM",
    ];
    let mut parts = vec![rng.pick(&["SELECT", "UPDATE", "INSERT", "x"])];
    for _ in 0..rng.below(8) {
        parts.push(rng.pick(PIECES));
    }
    parts.join(" ")
}

#[test]
fn generated_secrets_are_masked_and_the_rest_is_kept() {
    for seed in 0..1500 {
        let mut rng = Rng::new(seed);
        for dialect in ALL {
            let marker = format!("S3cr3t{seed}");
            let templates = secret_templates(dialect);
            let secret = templates[rng.below(templates.len())].replace("{s}", &marker);
            let separator = if dialect == TSQL { "\nGO\n" } else { ";\n" };
            let prefix: Vec<String> = (0..rng.below(3))
                .map(|_| benign_statement(&mut rng))
                .collect();
            let suffix: Vec<String> = (0..rng.below(3))
                .map(|_| benign_statement(&mut rng))
                .collect();
            let mut head = String::new();
            for statement in &prefix {
                head.push_str(statement);
                head.push_str(separator);
            }
            let mut tail = String::new();
            for statement in &suffix {
                tail.push_str(separator);
                tail.push_str(statement);
            }
            let script = format!("{head}{secret}{tail}");
            let context = format!("{dialect:?} seed {seed}: {script:?}");
            let out = redact_secrets(dialect, &script);
            assert!(matches!(out, Cow::Owned(_)), "not redacted: {context}");
            assert!(!out.contains(&marker), "secret kept: {context} -> {out:?}");
            assert!(
                out.starts_with(&head),
                "prefix changed: {context} -> {out:?}"
            );
            assert!(out.ends_with(&tail), "suffix changed: {context} -> {out:?}");
            // The masked text still splits into the same statements.
            assert_eq!(
                split(dialect, &out).len(),
                split(dialect, &script).len(),
                "{context} -> {out:?}"
            );
            // Benign statements alone are never touched.
            let benign = format!("{head}{}", tail.trim_start_matches(separator));
            assert!(
                matches!(redact_secrets(dialect, &benign), Cow::Borrowed(_)),
                "benign text changed: {dialect:?} {benign:?}"
            );
        }
    }
}
