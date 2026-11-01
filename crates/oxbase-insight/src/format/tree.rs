//! Parentheses and `CASE ... END` blocks of a statement as a tree over its items.

use super::prep::Item;

/// The deepest nesting of parentheses and `CASE` blocks the formatter lays out; deeper
/// statements are kept as typed (the layout recurses once per level).
const MAX_DEPTH: usize = 200;

/// One node: an item, a parenthesised group or a `CASE` block (indices into the items).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    Tok(usize),
    Paren {
        open: usize,
        close: usize,
        kids: Vec<Node>,
    },
    Case {
        case: usize,
        end: usize,
        kids: Vec<Node>,
    },
}

impl Node {
    /// The first item of the node.
    pub fn first(&self) -> usize {
        match self {
            Node::Tok(i) => *i,
            Node::Paren { open, .. } => *open,
            Node::Case { case, .. } => *case,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Open {
    Paren(usize),
    Case(usize),
}

/// The tree of `items`, or `None` when parentheses or `CASE` / `END` do not balance (or nest
/// deeper than [`MAX_DEPTH`]): such a statement is kept as typed.
pub(super) fn tree(items: &[Item<'_>]) -> Option<Vec<Node>> {
    let mut stack: Vec<(Option<Open>, Vec<Node>)> = vec![(None, Vec::new())];
    for (i, it) in items.iter().enumerate() {
        let opens = if it.is_punct("(") {
            Some(Open::Paren(i))
        } else if it.is_word("CASE") {
            Some(Open::Case(i))
        } else {
            None
        };
        if let Some(open) = opens {
            if stack.len() > MAX_DEPTH {
                return None;
            }
            stack.push((Some(open), Vec::new()));
            continue;
        }
        let closes_paren = if it.is_punct(")") {
            true
        } else if it.is_word("END") {
            false
        } else {
            stack.last_mut()?.1.push(Node::Tok(i));
            continue;
        };
        let (open, kids) = stack.pop()?;
        let node = match (open?, closes_paren) {
            (Open::Paren(open), true) => Node::Paren {
                open,
                close: i,
                kids,
            },
            (Open::Case(case), false) => Node::Case { case, end: i, kids },
            _ => return None,
        };
        stack.last_mut()?.1.push(node);
    }
    let (open, kids) = stack.pop()?;
    if open.is_some() || !stack.is_empty() {
        return None;
    }
    Some(kids)
}

#[cfg(test)]
mod tests {
    use oxbase_sql::SqlDialect;

    use super::super::prep::prepare;
    use super::*;
    use crate::SqlLanguage;

    fn parse(text: &str) -> Option<Vec<Node>> {
        let items = prepare(SqlDialect::Postgres, text, &SqlLanguage::standard()).unwrap();
        tree(&items)
    }

    #[test]
    fn balanced_groups() {
        let nodes = parse("select (a + (b)), case when x then 1 end from t").unwrap();
        assert_eq!(nodes.len(), 6);
        assert!(matches!(&nodes[1], Node::Paren { kids, .. } if kids.len() == 3));
        assert!(matches!(&nodes[3], Node::Case { kids, .. } if kids.len() == 4));
        assert_eq!(nodes[3].first(), 9);
    }

    #[test]
    fn unbalanced_groups_are_refused() {
        assert_eq!(parse("select (a"), None);
        assert_eq!(parse("select a)"), None);
        assert_eq!(parse("select case when a then b"), None);
        assert_eq!(parse("select (case when a then b) end"), None);
        assert_eq!(parse("select end"), None);
        let deep = format!("select {}1{}", "(".repeat(300), ")".repeat(300));
        assert_eq!(parse(&deep), None);
    }
}
