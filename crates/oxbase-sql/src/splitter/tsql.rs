//! T-SQL statement tracking inside one `GO` batch: which `;` may end a statement.
//!
//! [`Tracker`] sees every content token of the batch (comments and whitespace are not
//! content) and answers [`Tracker::can_cut`] when the splitter reaches a `;`. A `;` ends a
//! statement only outside every block and outside a module definition:
//!
//! - `BEGIN ... END`, `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` (one block from
//!   `BEGIN TRY` to `END CATCH`, so a `;` between `END TRY` and `BEGIN CATCH` is inside) and
//!   `CASE ... END`. `BEGIN TRAN[SACTION]`, `BEGIN DISTRIBUTED ...`, `BEGIN DIALOG ...` and
//!   `BEGIN CONVERSATION TIMER` are statements, not blocks; `END CONVERSATION` is a statement,
//!   not the end of a block.
//! - A statement that starts with `CREATE [OR ALTER] {PROC|PROCEDURE|FUNCTION|TRIGGER|VIEW}`,
//!   `ALTER {PROC|PROCEDURE|FUNCTION|TRIGGER|VIEW}` or `CREATE {RULE|DEFAULT}` runs to the end
//!   of the batch: the server takes the rest of the batch as the module body.
//!
//! It also records whether the batch holds **batch-scoped state** outside module bodies:
//! a local variable (`@name`, not `@@name`), `DECLARE`, `SCOPE_IDENTITY`, `@@ROWCOUNT`,
//! `@@ERROR`, `GOTO` or `RETURN`. Such a batch is sent whole (see [`Tracker::scoped`]),
//! because running its statements one by one would lose the variables, the scope or the
//! control flow they share.

/// Words the tracker reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kw {
    Alter,
    Begin,
    Case,
    Catch,
    Create,
    /// `RULE`, `DEFAULT`: modules after `CREATE` only.
    CreateOnlyModule,
    Else,
    End,
    /// `PROC`, `PROCEDURE`, `FUNCTION`, `TRIGGER`, `VIEW`.
    Module,
    /// After `BEGIN`, a statement rather than a block: `TRAN`, `TRANSACTION`, `DISTRIBUTED`,
    /// `DIALOG`, `CONVERSATION` (also `END CONVERSATION`).
    NotBlock,
    Or,
    /// Batch-scoped state: `DECLARE`, `SCOPE_IDENTITY`, `@@ROWCOUNT`, `@@ERROR`, `GOTO`,
    /// `RETURN`, or a local variable `@name`.
    Scope,
    Try,
    Other,
}

impl Kw {
    pub(super) fn of(word: &[u8]) -> Self {
        let is = |k: &str| word.eq_ignore_ascii_case(k.as_bytes());
        if let [b'@', second, ..] = word
            && *second != b'@'
        {
            return Self::Scope;
        }
        if is("BEGIN") {
            Self::Begin
        } else if is("END") {
            Self::End
        } else if is("CASE") {
            Self::Case
        } else if is("ELSE") {
            Self::Else
        } else if is("TRY") {
            Self::Try
        } else if is("CATCH") {
            Self::Catch
        } else if is("CREATE") {
            Self::Create
        } else if is("ALTER") {
            Self::Alter
        } else if is("OR") {
            Self::Or
        } else if is("PROC") || is("PROCEDURE") || is("FUNCTION") || is("TRIGGER") || is("VIEW") {
            Self::Module
        } else if is("RULE") || is("DEFAULT") {
            Self::CreateOnlyModule
        } else if is("TRAN")
            || is("TRANSACTION")
            || is("DISTRIBUTED")
            || is("DIALOG")
            || is("CONVERSATION")
        {
            Self::NotBlock
        } else if is("DECLARE")
            || is("SCOPE_IDENTITY")
            || is("@@ROWCOUNT")
            || is("@@ERROR")
            || is("GOTO")
            || is("RETURN")
        {
            Self::Scope
        } else {
            Self::Other
        }
    }
}

/// A content token as the tracker sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Word(Kw),
    /// A one-byte punctuation token.
    Punct(u8),
    /// Strings, quoted identifiers.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Begin,
    Case,
    /// `BEGIN TRY`, until its `END TRY`.
    Try,
    /// After `END TRY`, until `BEGIN CATCH`.
    AwaitCatch,
    /// `BEGIN CATCH`, until its `END CATCH`.
    Catch,
}

/// The previous token needs the next one to be understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    /// `BEGIN` opened a block; `TRAN`, `TRY`, `CATCH`, ... may change what it is.
    AfterBegin,
    /// `END` (not of a `CASE`); `TRY`, `CATCH`, `CONVERSATION` may follow.
    AfterEnd,
}

/// How far the start of the current statement has been recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    Start,
    Create,
    CreateOr,
    /// `ALTER` or `CREATE OR ALTER`: waiting for the object keyword.
    Alter,
    /// A module definition: no `;` ends it.
    Module,
    Plain,
}

/// Block and module tracking for one batch (see the module docs).
#[derive(Debug)]
pub(super) struct Tracker {
    stack: Vec<Block>,
    pending: Pending,
    header: Header,
    prev_dot: bool,
    scoped: bool,
}

impl Tracker {
    pub(super) const fn new() -> Self {
        Self {
            stack: Vec::new(),
            pending: Pending::None,
            header: Header::Start,
            prev_dot: false,
            scoped: false,
        }
    }

    /// Forgets everything: a new batch starts.
    pub(super) fn reset(&mut self) {
        self.stack.clear();
        self.pending = Pending::None;
        self.header = Header::Start;
        self.prev_dot = false;
        self.scoped = false;
    }

    /// A `;` ended the current statement; the next content token starts a new one.
    pub(super) fn new_statement(&mut self) {
        self.header = Header::Start;
        self.prev_dot = false;
    }

    /// A cut was taken back (`... ; ELSE`): the statement goes on, as an ordinary statement.
    pub(super) fn resume_statement(&mut self) {
        self.header = Header::Plain;
    }

    /// Whether the batch holds batch-scoped state outside module bodies.
    pub(super) const fn scoped(&self) -> bool {
        self.scoped
    }

    /// Whether the `;` just fed ends the current statement.
    pub(super) fn can_cut(&self) -> bool {
        self.stack.is_empty() && self.header != Header::Module
    }

    pub(super) fn feed(&mut self, signal: Signal) {
        let signal = match signal {
            Signal::Word(_) if self.prev_dot => Signal::Word(Kw::Other),
            _ => signal,
        };
        self.prev_dot = signal == Signal::Punct(b'.');
        self.header(signal);
        if let Signal::Word(Kw::Scope) = signal
            && self.header != Header::Module
        {
            self.scoped = true;
        }
        let kw = match signal {
            Signal::Word(kw) => kw,
            Signal::Punct(_) | Signal::Other => Kw::Other,
        };
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::None => {}
            Pending::AfterBegin => match kw {
                Kw::NotBlock => {
                    self.stack.pop();
                    return;
                }
                Kw::Try => {
                    self.replace_top(Block::Try);
                    return;
                }
                Kw::Catch => {
                    self.stack.pop();
                    if self.stack.last() == Some(&Block::AwaitCatch) {
                        self.replace_top(Block::Catch);
                    } else {
                        self.stack.push(Block::Catch);
                    }
                    return;
                }
                _ => {}
            },
            Pending::AfterEnd => match kw {
                // `END CONVERSATION`: a statement.
                Kw::NotBlock => return,
                Kw::Try => {
                    if self.stack.last() == Some(&Block::Try) {
                        self.replace_top(Block::AwaitCatch);
                    } else {
                        self.stack.pop();
                    }
                    return;
                }
                Kw::Catch => {
                    self.stack.pop();
                    return;
                }
                _ => {
                    self.stack.pop();
                }
            },
        }
        match kw {
            Kw::Begin => {
                self.stack.push(Block::Begin);
                self.pending = Pending::AfterBegin;
            }
            Kw::Case => self.stack.push(Block::Case),
            Kw::End => match self.stack.last() {
                Some(Block::Case) => {
                    self.stack.pop();
                }
                Some(_) => self.pending = Pending::AfterEnd,
                None => {}
            },
            _ => {}
        }
    }

    fn replace_top(&mut self, block: Block) {
        if let Some(top) = self.stack.last_mut() {
            *top = block;
        } else {
            self.stack.push(block);
        }
    }

    fn header(&mut self, signal: Signal) {
        let kw = match signal {
            Signal::Word(kw) => kw,
            Signal::Punct(_) | Signal::Other => Kw::Other,
        };
        self.header = match (self.header, kw) {
            (Header::Module, _) => Header::Module,
            (Header::Start, Kw::Create) => Header::Create,
            (Header::Start | Header::CreateOr, Kw::Alter) => Header::Alter,
            (Header::Create, Kw::Or) => Header::CreateOr,
            (Header::Create, Kw::Module | Kw::CreateOnlyModule) | (Header::Alter, Kw::Module) => {
                Header::Module
            }
            _ => Header::Plain,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(tracker: &mut Tracker, text: &str) {
        for word in text.split_whitespace() {
            let signal = match word.as_bytes() {
                [b] if !b.is_ascii_alphanumeric() && *b != b'@' => Signal::Punct(*b),
                bytes => Signal::Word(Kw::of(bytes)),
            };
            tracker.feed(signal);
        }
    }

    #[test]
    fn keywords() {
        assert_eq!(Kw::of(b"begin"), Kw::Begin);
        assert_eq!(Kw::of(b"@x"), Kw::Scope);
        assert_eq!(Kw::of(b"@@rowcount"), Kw::Scope);
        assert_eq!(Kw::of(b"@@VERSION"), Kw::Other);
        assert_eq!(Kw::of(b"@"), Kw::Other);
        assert_eq!(Kw::of(b"Procedure"), Kw::Module);
        assert_eq!(Kw::of(b"PROCEDURES"), Kw::Other);
    }

    #[test]
    fn blocks_and_statements() {
        let cases: &[(&str, bool)] = &[
            ("BEGIN SELECT 1 ;", false),
            ("BEGIN SELECT 1 ; END ;", true),
            ("BEGIN TRAN ;", true),
            ("BEGIN DISTRIBUTED TRANSACTION ;", true),
            ("BEGIN TRY SELECT 1 ; END TRY ;", false),
            (
                "BEGIN TRY SELECT 1 ; END TRY BEGIN CATCH SELECT 2 ; END CATCH ;",
                true,
            ),
            ("BEGIN END CONVERSATION x ;", false),
            ("BEGIN END CONVERSATION x ; END ;", true),
            ("SELECT CASE WHEN 1 THEN 2 END ;", true),
            ("BEGIN SELECT CASE WHEN 1 THEN 2 END ; END ;", true),
            ("BEGIN BEGIN SELECT 1 ; END ;", false),
            ("CREATE PROCEDURE p AS SELECT 1 ;", false),
            ("CREATE OR ALTER VIEW v AS SELECT 1 ;", false),
            ("ALTER PROC p AS SELECT 1 ;", false),
            ("CREATE DEFAULT d AS 0 ;", false),
            ("ALTER TABLE t ADD DEFAULT 0 FOR c ;", true),
            ("CREATE TABLE t ( a int ) ;", true),
            ("x . end ;", true),
        ];
        for (text, cut) in cases {
            let mut tracker = Tracker::new();
            words(&mut tracker, text);
            assert_eq!(tracker.can_cut(), *cut, "{text}");
        }
    }

    #[test]
    fn scope_markers_outside_modules() {
        let mut tracker = Tracker::new();
        words(&mut tracker, "SELECT @@VERSION ;");
        assert!(!tracker.scoped());
        words(&mut tracker, "SELECT @x ;");
        assert!(tracker.scoped());
        let mut tracker = Tracker::new();
        words(
            &mut tracker,
            "CREATE PROCEDURE p @a int AS DECLARE @b int ;",
        );
        assert!(!tracker.scoped());
    }
}
