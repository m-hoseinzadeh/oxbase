//! A small pretty printer: the layout builds a flat list of [`D`] commands and [`render`] turns
//! it into text.
//!
//! A group (`Open` ... `Close`) is laid out flat (its `Line`s become spaces, its `SoftLine`s
//! nothing) when all of it fits on the current line, and broken (its `Line`s and `SoftLine`s
//! become line breaks) otherwise. A `Hard` line break, or a text holding a line break, never
//! fits, so every group around it breaks. Spaces and line breaks are emitted lazily: several
//! requests in a row collapse into one, a line break wins over a space, and nothing trails at
//! the end of a line or of the output.

use std::borrow::Cow;

/// One layout command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum D<'a> {
    Text(Cow<'a, str>),
    /// A space (never at the start of a line).
    Space,
    /// A space when the enclosing group is flat, a line break when it is broken.
    Line,
    /// Nothing when the enclosing group is flat, a line break when it is broken.
    SoftLine,
    /// Always a line break.
    Hard,
    Open,
    Close,
    /// One more indent level for the line breaks that follow.
    Indent,
    Dedent,
}

/// Where and how the output is laid out.
#[derive(Debug, Clone, Copy)]
pub(super) struct Layout<'p> {
    /// Soft line length limit (columns).
    pub max: usize,
    /// One indent level.
    pub unit: &'p str,
    /// Columns of one indent level.
    pub unit_width: usize,
    /// Written at the start of every new line, before the indent (`format_range`).
    pub prefix: &'p str,
    pub prefix_width: usize,
    /// The column the first line starts at.
    pub start_col: usize,
    pub eol: &'p str,
}

/// Width that never fits.
const INF: usize = usize::MAX / 4;

fn add(a: usize, b: usize) -> usize {
    a.saturating_add(b).min(INF)
}

/// Columns of a text; [`INF`] when it spans lines.
fn width(text: &str) -> usize {
    if text.contains('\n') {
        INF
    } else {
        text.chars().count()
    }
}

/// The flat width of every group, at the index of its `Open`.
fn group_widths(docs: &[D<'_>]) -> Vec<usize> {
    let mut widths = vec![0; docs.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let w = match d {
            D::Text(t) => width(t),
            D::Space | D::Line => 1,
            D::Hard => {
                // A line break that ends groups (a trailing line comment) breaks only the
                // groups that go on after it.
                let ending = docs[i + 1..]
                    .iter()
                    .filter(|d| !matches!(d, D::Indent | D::Dedent))
                    .take_while(|d| matches!(d, D::Close))
                    .count();
                let keep = stack.len().saturating_sub(ending);
                for entry in &mut stack[..keep] {
                    entry.1 = INF;
                }
                continue;
            }
            D::SoftLine | D::Indent | D::Dedent => 0,
            D::Open => {
                stack.push((i, 0));
                continue;
            }
            D::Close => {
                let Some((open, w)) = stack.pop() else {
                    continue;
                };
                widths[open] = w;
                w
            }
        };
        if let Some(top) = stack.last_mut() {
            top.1 = add(top.1, w);
        }
    }
    widths
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Nothing,
    Space,
    /// A line break, then the indent of this level.
    Newline(usize),
}

struct Renderer<'l> {
    layout: &'l Layout<'l>,
    out: String,
    col: usize,
    pending: Pending,
    level: usize,
    /// Per open group: broken?
    groups: Vec<bool>,
    started: bool,
}

impl Renderer<'_> {
    fn broken(&self) -> bool {
        self.groups.last().copied().unwrap_or(true)
    }

    fn space(&mut self) {
        if self.started && self.pending == Pending::Nothing {
            self.pending = Pending::Space;
        }
    }

    fn newline(&mut self) {
        if self.started {
            self.pending = Pending::Newline(self.level);
        }
    }

    fn indent_width(&self, level: usize) -> usize {
        add(
            self.layout.prefix_width,
            level.saturating_mul(self.layout.unit_width),
        )
    }

    /// The column the next text would start at.
    fn next_col(&self) -> usize {
        match self.pending {
            Pending::Nothing => self.col,
            Pending::Space => add(self.col, 1),
            Pending::Newline(level) => self.indent_width(level),
        }
    }

    fn text(&mut self, text: &str) {
        match self.pending {
            Pending::Nothing => {}
            Pending::Space => {
                self.out.push(' ');
                self.col = add(self.col, 1);
            }
            Pending::Newline(level) => {
                self.out.push_str(self.layout.eol);
                self.out.push_str(self.layout.prefix);
                for _ in 0..level {
                    self.out.push_str(self.layout.unit);
                }
                self.col = self.indent_width(level);
            }
        }
        self.pending = Pending::Nothing;
        self.out.push_str(text);
        self.col = match text.rfind('\n') {
            Some(p) => text[p + 1..].chars().count(),
            None => add(self.col, text.chars().count()),
        };
        self.started = true;
    }
}

/// Lays `docs` out (see the module docs).
pub(super) fn render(docs: &[D<'_>], layout: &Layout<'_>) -> String {
    let widths = group_widths(docs);
    let mut out = Renderer {
        layout,
        out: String::new(),
        col: layout.start_col,
        pending: Pending::Nothing,
        level: 0,
        groups: Vec::new(),
        started: false,
    };
    for (i, d) in docs.iter().enumerate() {
        match d {
            D::Text(text) => out.text(text),
            D::Space => out.space(),
            D::Hard => out.newline(),
            D::Line => {
                if out.broken() {
                    out.newline();
                } else {
                    out.space();
                }
            }
            D::SoftLine => {
                if out.broken() {
                    out.newline();
                }
            }
            D::Open => {
                let fits = add(out.next_col(), widths[i]) <= layout.max;
                out.groups.push(!fits);
            }
            D::Close => {
                out.groups.pop();
            }
            D::Indent => out.level += 1,
            D::Dedent => out.level = out.level.saturating_sub(1),
        }
    }
    out.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> D<'_> {
        D::Text(Cow::Borrowed(s))
    }

    const LAYOUT: Layout<'static> = Layout {
        max: 20,
        unit: "  ",
        unit_width: 2,
        prefix: "",
        prefix_width: 0,
        start_col: 0,
        eol: "\n",
    };

    #[test]
    fn groups_break_only_when_too_long() {
        let short = [
            D::Open,
            t("f("),
            D::Indent,
            D::SoftLine,
            t("a,"),
            D::Line,
            t("b"),
            D::Dedent,
            D::SoftLine,
            t(")"),
            D::Close,
        ];
        assert_eq!(render(&short, &LAYOUT), "f(a, b)");
        let long = [
            D::Open,
            t("f("),
            D::Indent,
            D::SoftLine,
            t("aaaaaaaaaa,"),
            D::Line,
            t("bbbbbbbbbb"),
            D::Dedent,
            D::SoftLine,
            t(")"),
            D::Close,
        ];
        assert_eq!(render(&long, &LAYOUT), "f(\n  aaaaaaaaaa,\n  bbbbbbbbbb\n)");
    }

    #[test]
    fn hard_breaks_force_groups_and_collapse() {
        let docs = [
            D::Open,
            t("a"),
            D::Space,
            t("-- c"),
            D::Hard,
            D::Line,
            D::Hard,
            t("b"),
            D::Close,
            D::Hard,
            D::Space,
        ];
        assert_eq!(render(&docs, &LAYOUT), "a -- c\nb");
        let prefixed = Layout {
            prefix: "\t",
            prefix_width: 4,
            start_col: 4,
            eol: "\r\n",
            ..LAYOUT
        };
        assert_eq!(
            render(&[t("x"), D::Indent, D::Hard, t("y")], &prefixed),
            "x\r\n\t  y"
        );
    }
}
