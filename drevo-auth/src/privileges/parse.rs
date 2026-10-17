//! Reading `GRANT`, `DENY` and `REVOKE` privilege commands.

use std::fmt;

use super::model::{Access, Action, Level, Pattern, Privilege, Resource, Segment, Target};

/// What a privilege command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `GRANT … TO roles`.
    Grant,
    /// `DENY … TO roles`.
    Deny,
    /// `REVOKE [GRANT | DENY] … FROM roles`: removes the grant, the deny,
    /// or (with neither word) both.
    Revoke(Option<Access>),
}

/// A parsed privilege command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Grant, deny or revoke.
    pub op: Op,
    /// The privileges, one per graph, element, property, label or name the
    /// command lists.
    pub privileges: Vec<Privilege>,
    /// The roles, in the order given.
    pub roles: Vec<String>,
}

/// Why a command could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(message.into()))
}

/// Words that end a list of names; a name spelled like one needs backticks.
const RESERVED: [&str; 11] = [
    "TO",
    "FROM",
    "ON",
    "NODE",
    "NODES",
    "RELATIONSHIP",
    "RELATIONSHIPS",
    "ELEMENT",
    "ELEMENTS",
    "WHERE",
    "FOR",
];

/// Whether `word` must be quoted to be read as a name.
pub(crate) fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A bare word, or a name in backticks (`quoted`).
    Word {
        text: String,
        quoted: bool,
    },
    Star,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    /// Any other character: reported where the grammar does not expect it.
    Other(char),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word { text, quoted: true } => write!(f, "`{text}`"),
            Self::Word { text, .. } => f.write_str(text),
            Self::Star => f.write_str("*"),
            Self::LBrace => f.write_str("{"),
            Self::RBrace => f.write_str("}"),
            Self::LParen => f.write_str("("),
            Self::RParen => f.write_str(")"),
            Self::Comma => f.write_str(","),
            Self::Other(c) => write!(f, "{c}"),
        }
    }
}

fn lex(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '{' | '}' | '(' | ')' | ',' => {
                chars.next();
                out.push(match c {
                    '{' => Token::LBrace,
                    '}' => Token::RBrace,
                    '(' => Token::LParen,
                    ')' => Token::RParen,
                    _ => Token::Comma,
                });
            }
            '`' => {
                chars.next();
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('`') if chars.peek() == Some(&'`') => {
                            chars.next();
                            name.push('`');
                        }
                        Some('`') => break,
                        Some(ch) => name.push(ch),
                        None => return err("a name in backticks has no closing backtick"),
                    }
                }
                if name.is_empty() {
                    return err("a name in backticks is empty");
                }
                out.push(Token::Word {
                    text: name,
                    quoted: true,
                });
            }
            c if is_word_char(c) => {
                let mut word = String::new();
                while let Some(&ch) = chars.peek() {
                    if !is_word_char(ch) {
                        break;
                    }
                    word.push(ch);
                    chars.next();
                }
                out.push(if word == "*" {
                    Token::Star
                } else {
                    Token::Word {
                        text: word,
                        quoted: false,
                    }
                });
            }
            other => {
                chars.next();
                out.push(Token::Other(other));
            }
        }
    }
    Ok(out)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '*' | '?' | '$')
}

/// Read a `GRANT`, `DENY` or `REVOKE` privilege command.
///
/// ```
/// use drevo_auth::privileges::{parse, Op};
///
/// let c = parse("GRANT MATCH {*} ON GRAPH tasks NODES Task, Bug TO reviewer").unwrap();
/// assert_eq!(c.op, Op::Grant);
/// assert_eq!(c.roles, ["reviewer"]);
/// assert_eq!(c.privileges[1].to_string(), "MATCH {*} ON GRAPH tasks NODE Bug");
/// ```
///
/// # Errors
///
/// A [`ParseError`] that says what was expected where.
pub fn parse(text: &str) -> Result<Command, ParseError> {
    let tokens = lex(text)?;
    let mut p = Parser { tokens, pos: 0 };
    let command = p.command()?;
    if let Some(t) = p.peek() {
        return err(format!("unexpected '{t}' after the command"));
    }
    Ok(command)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

/// The words of a privilege before `ON`, with its argument.
struct Head {
    action: Action,
    /// Properties (`{a, b}`) or labels.
    names: Vec<Pattern>,
    /// Users of `TRANSACTION MANAGEMENT (…)`, or procedure globs.
    qualifiers: Vec<Pattern>,
    /// `ALL [x] PRIVILEGES` names its level only through `ON`.
    all: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    /// Whether the next token is the bare keyword `kw`.
    fn at(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word { text, quoted: false }) if text.eq_ignore_ascii_case(kw))
    }

    fn eat(&mut self, kw: &str) -> bool {
        if self.at(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_any(&mut self, kws: &[&str]) -> Option<String> {
        for kw in kws {
            if self.eat(kw) {
                return Some(kw.to_string());
            }
        }
        None
    }

    fn expect(&mut self, kw: &str, context: &str) -> Result<(), ParseError> {
        if self.eat(kw) {
            Ok(())
        } else {
            err(format!("expected {kw} {context}, found {}", self.found()))
        }
    }

    fn found(&self) -> String {
        match self.peek() {
            Some(t) => format!("'{t}'"),
            None => "the end of the command".to_string(),
        }
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        let op = if self.eat("GRANT") {
            Op::Grant
        } else if self.eat("DENY") {
            Op::Deny
        } else if self.eat("REVOKE") {
            if self.eat("GRANT") {
                Op::Revoke(Some(Access::Grant))
            } else if self.eat("DENY") {
                Op::Revoke(Some(Access::Deny))
            } else {
                Op::Revoke(None)
            }
        } else {
            return err(format!(
                "expected GRANT, DENY or REVOKE, found {}",
                self.found()
            ));
        };
        if self.at("IMMUTABLE") {
            return err("IMMUTABLE privileges are not supported");
        }
        let privileges = self.privilege()?;
        let (word, other) = match op {
            Op::Revoke(_) => ("FROM", "TO"),
            _ => ("TO", "FROM"),
        };
        if !self.eat(word) {
            let hint = if self.at(other) {
                format!(
                    " ({op_name} uses {word}, not {other})",
                    op_name = op_name(op)
                )
            } else {
                String::new()
            };
            return err(format!(
                "expected {word} and a role after the privilege, found {}{hint}",
                self.found()
            ));
        }
        let roles = self.name_list("a role name")?;
        Ok(Command {
            op,
            privileges,
            roles,
        })
    }

    /// One name: a bare word or a name in backticks.
    fn name(&mut self, what: &str) -> Result<String, ParseError> {
        match self.next() {
            Some(Token::Word { text, quoted: true }) => Ok(text),
            Some(Token::Word {
                text,
                quoted: false,
            }) if !is_reserved(&text) => {
                if text.contains(['*', '?']) {
                    return err(format!("expected {what}, found '{text}'"));
                }
                Ok(text)
            }
            Some(t) => {
                self.pos -= 1;
                err(format!("expected {what}, found '{t}'"))
            }
            None => err(format!("expected {what}, found the end of the command")),
        }
    }

    fn name_list(&mut self, what: &str) -> Result<Vec<String>, ParseError> {
        let mut names = vec![self.name(what)?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.pos += 1;
            names.push(self.name(what)?);
        }
        Ok(names)
    }

    /// `*` or a list of names.
    fn patterns(&mut self, what: &str) -> Result<Vec<Pattern>, ParseError> {
        if matches!(self.peek(), Some(Token::Star)) {
            self.pos += 1;
            return Ok(vec![Pattern::All]);
        }
        Ok(self
            .name_list(what)?
            .into_iter()
            .map(Pattern::Name)
            .collect())
    }

    /// Procedure or function globs: words that may hold `.`, `*` and `?`.
    fn globs(&mut self) -> Result<Vec<Pattern>, ParseError> {
        let mut out = Vec::new();
        loop {
            match self.next() {
                Some(Token::Star) => out.push(Pattern::All),
                Some(Token::Word { text, quoted })
                    if quoted || !text.eq_ignore_ascii_case("ON") =>
                {
                    out.push(Pattern::Name(text));
                }
                _ => {
                    self.pos -= 1;
                    return err(format!(
                        "expected a procedure or function name, found {}",
                        self.found()
                    ));
                }
            }
            if matches!(self.peek(), Some(Token::Comma)) {
                self.pos += 1;
            } else {
                return Ok(out);
            }
        }
    }

    /// `{*}` or `{name, …}`.
    fn properties(&mut self) -> Result<Vec<Pattern>, ParseError> {
        if !matches!(self.peek(), Some(Token::LBrace)) {
            return err(format!(
                "expected a property list in braces, e.g. {{*}} or {{name}}, found {}",
                self.found()
            ));
        }
        self.pos += 1;
        let props = self.patterns("a property name")?;
        if !matches!(self.next(), Some(Token::RBrace)) {
            self.pos -= 1;
            return err(format!(
                "expected }} after the properties, found {}",
                self.found()
            ));
        }
        Ok(props)
    }

    fn head(&mut self) -> Result<Head, ParseError> {
        let simple = |action| Head {
            action,
            names: Vec::new(),
            qualifiers: Vec::new(),
            all: false,
        };
        let with_names = |action, names| Head {
            action,
            names,
            qualifiers: Vec::new(),
            all: false,
        };
        if self.eat("TRAVERSE") {
            return Ok(simple(Action::Traverse));
        }
        if self.eat("READ") {
            return Ok(with_names(Action::Read, self.properties()?));
        }
        if self.eat("MATCH") {
            return Ok(with_names(Action::Match, self.properties()?));
        }
        if self.eat("MERGE") {
            return Ok(with_names(Action::Merge, self.properties()?));
        }
        if self.eat("CREATE") {
            return Ok(simple(Action::Create));
        }
        if self.eat("DELETE") {
            return Ok(simple(Action::Delete));
        }
        if self.eat("WRITE") {
            return Ok(simple(Action::Write));
        }
        if self.eat("SET") {
            if self.eat("LABEL") {
                return Ok(with_names(Action::SetLabel, self.patterns("a label")?));
            }
            if self.eat("PROPERTY") {
                return Ok(with_names(Action::SetProperty, self.properties()?));
            }
            return err(format!(
                "expected LABEL or PROPERTY after SET, found {}",
                self.found()
            ));
        }
        if self.eat("REMOVE") {
            self.expect("LABEL", "after REMOVE")?;
            return Ok(with_names(Action::RemoveLabel, self.patterns("a label")?));
        }
        if self.eat("ALL") {
            let _ = self.eat_any(&["GRAPH", "DATABASE", "DBMS"]);
            let _ = self.eat("PRIVILEGES");
            let mut h = simple(Action::AllGraph);
            h.all = true;
            return Ok(h);
        }
        if self.eat("ACCESS") {
            return Ok(simple(Action::Access));
        }
        if self.eat("START") {
            return Ok(simple(Action::Start));
        }
        if self.eat("STOP") {
            return Ok(simple(Action::Stop));
        }
        if self.eat_any(&["INDEX", "INDEXES"]).is_some() {
            let _ = self.eat("MANAGEMENT");
            return Ok(simple(Action::IndexManagement));
        }
        if self.eat_any(&["CONSTRAINT", "CONSTRAINTS"]).is_some() {
            let _ = self.eat("MANAGEMENT");
            return Ok(simple(Action::ConstraintManagement));
        }
        if self.eat("NAME") {
            let _ = self.eat("MANAGEMENT");
            return Ok(simple(Action::NameManagement));
        }
        if self.eat("TRANSACTION") {
            let _ = self.eat("MANAGEMENT");
            let users = if matches!(self.peek(), Some(Token::LParen)) {
                self.pos += 1;
                let users = self.patterns("a user name")?;
                if !matches!(self.next(), Some(Token::RParen)) {
                    self.pos -= 1;
                    return err(format!(
                        "expected ) after the users, found {}",
                        self.found()
                    ));
                }
                users
            } else {
                vec![Pattern::All]
            };
            return Ok(Head {
                action: Action::TransactionManagement,
                names: Vec::new(),
                qualifiers: users,
                all: false,
            });
        }
        for (word, action) in [
            ("USER", Action::UserManagement),
            ("ROLE", Action::RoleManagement),
            ("PRIVILEGE", Action::PrivilegeManagement),
            ("DATABASE", Action::DatabaseManagement),
        ] {
            if self.at(word)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Word { text, quoted: false }) if text.eq_ignore_ascii_case("MANAGEMENT"))
            {
                self.pos += 2;
                return Ok(simple(action));
            }
        }
        if self.eat("EXECUTE") {
            if self.eat_any(&["ADMIN", "ADMINISTRATOR"]).is_some() {
                self.expect("PROCEDURES", "after EXECUTE ADMIN")?;
                return Ok(simple(Action::ExecuteAdminProcedures));
            }
            let action = if self.eat_any(&["PROCEDURE", "PROCEDURES"]).is_some() {
                Action::ExecuteProcedure
            } else if self.eat_any(&["FUNCTION", "FUNCTIONS"]).is_some() {
                Action::ExecuteFunction
            } else {
                return err(format!(
                    "expected PROCEDURE, FUNCTION or ADMIN PROCEDURES after EXECUTE, found {}",
                    self.found()
                ));
            };
            return Ok(Head {
                action,
                names: Vec::new(),
                qualifiers: self.globs()?,
                all: false,
            });
        }
        err(format!("expected a privilege, found {}", self.found()))
    }

    /// The privileges between the command word and `TO` / `FROM`.
    fn privilege(&mut self) -> Result<Vec<Privilege>, ParseError> {
        let mut head = self.head()?;
        self.expect("ON", "after the privilege")?;
        // Where: the DBMS, databases or graphs.
        if self.eat("DBMS") {
            if head.all {
                head.action = Action::AllDbms;
            }
            if head.action.level() != Level::Dbms {
                return err(format!(
                    "{} applies to {}, not ON DBMS",
                    head.action.keyword(),
                    level_words(head.action.level())
                ));
            }
            return Ok(dbms_privileges(&head));
        }
        let (level, targets) = if self.eat("HOME") {
            let level = self.level_word()?;
            (level, vec![Target::Home])
        } else if self.at("DEFAULT") {
            return err("DEFAULT GRAPH / DEFAULT DATABASE is not supported; use HOME or a name");
        } else {
            let level = self.level_word()?;
            let targets = self
                .patterns(if level == Level::Graph {
                    "a graph name"
                } else {
                    "a database name"
                })?
                .into_iter()
                .map(|p| match p {
                    Pattern::All => Target::All,
                    Pattern::Name(n) => Target::Named(n),
                })
                .collect();
            (level, targets)
        };
        if head.all {
            head.action = if level == Level::Graph {
                Action::AllGraph
            } else {
                Action::AllDatabase
            };
        }
        if head.action.level() != level {
            return err(format!(
                "{} applies to {}, not ON {}",
                head.action.keyword(),
                level_words(head.action.level()),
                if level == Level::Graph {
                    "GRAPH"
                } else {
                    "DATABASE"
                }
            ));
        }
        if level == Level::Database {
            return Ok(database_privileges(&head, &targets));
        }
        // Graph: which elements.
        let segments = self.segments(head.action)?;
        if self.at("WHERE") || self.at("FOR") {
            return err("property-based segments (WHERE / FOR) are not supported");
        }
        Ok(graph_privileges(&head, &targets, &segments))
    }

    /// `GRAPH[S]` or `DATABASE[S]` after `ON` / `ON HOME`.
    fn level_word(&mut self) -> Result<Level, ParseError> {
        if self.eat_any(&["GRAPH", "GRAPHS"]).is_some() {
            Ok(Level::Graph)
        } else if self.eat_any(&["DATABASE", "DATABASES"]).is_some() {
            Ok(Level::Database)
        } else {
            err(format!(
                "expected GRAPH, DATABASE or DBMS after ON, found {}",
                self.found()
            ))
        }
    }

    /// `NODES …`, `RELATIONSHIPS …`, `ELEMENTS …`, or nothing (every
    /// element).
    fn segments(&mut self, action: Action) -> Result<Vec<Segment>, ParseError> {
        let kind = self.eat_any(&[
            "NODE",
            "NODES",
            "RELATIONSHIP",
            "RELATIONSHIPS",
            "ELEMENT",
            "ELEMENTS",
        ]);
        if !action.takes_segment() {
            if kind.is_some() {
                return err(format!(
                    "{} applies to the whole graph and takes no segment (NODES / RELATIONSHIPS / ELEMENTS)",
                    action.keyword()
                ));
            }
            return Ok(vec![Segment::Graph]);
        }
        let Some(kind) = kind else {
            return Ok(vec![
                Segment::Node(Pattern::All),
                Segment::Relationship(Pattern::All),
            ]);
        };
        let what = if kind.starts_with("NODE") {
            "a label"
        } else if kind.starts_with("RELATIONSHIP") {
            "a relationship type"
        } else {
            "a label or relationship type"
        };
        let patterns = self.patterns(what)?;
        let mut out = Vec::new();
        for p in patterns {
            if !kind.starts_with("RELATIONSHIP") {
                out.push(Segment::Node(p.clone()));
            }
            if !kind.starts_with("NODE") {
                out.push(Segment::Relationship(p));
            }
        }
        // ELEMENTS a, b lists every node segment before the relationships.
        out.sort_by_key(|s| matches!(s, Segment::Relationship(_)));
        Ok(out)
    }
}

fn op_name(op: Op) -> &'static str {
    match op {
        Op::Grant => "GRANT",
        Op::Deny => "DENY",
        Op::Revoke(_) => "REVOKE",
    }
}

fn level_words(level: Level) -> &'static str {
    match level {
        Level::Graph => "a GRAPH",
        Level::Database => "a DATABASE",
        Level::Dbms => "the DBMS",
    }
}

fn dbms_privileges(head: &Head) -> Vec<Privilege> {
    let make = |segment| Privilege {
        action: head.action,
        target: None,
        segment,
        resource: Resource::Database,
    };
    match head.action {
        Action::ExecuteProcedure => head
            .qualifiers
            .iter()
            .map(|p| make(Segment::Procedure(p.clone())))
            .collect(),
        Action::ExecuteFunction => head
            .qualifiers
            .iter()
            .map(|p| make(Segment::Function(p.clone())))
            .collect(),
        _ => vec![make(Segment::Database)],
    }
}

fn database_privileges(head: &Head, targets: &[Target]) -> Vec<Privilege> {
    let mut out = Vec::new();
    for target in targets {
        let make = |segment| Privilege {
            action: head.action,
            target: Some(target.clone()),
            segment,
            resource: Resource::Database,
        };
        if head.action == Action::TransactionManagement {
            out.extend(
                head.qualifiers
                    .iter()
                    .map(|u| make(Segment::User(u.clone()))),
            );
        } else {
            out.push(make(Segment::Database));
        }
    }
    out
}

fn graph_privileges(head: &Head, targets: &[Target], segments: &[Segment]) -> Vec<Privilege> {
    let resources: Vec<Resource> = if head.action.takes_properties() {
        head.names
            .iter()
            .map(|p| match p {
                Pattern::All => Resource::AllProperties,
                Pattern::Name(n) => Resource::Property(n.clone()),
            })
            .collect()
    } else if head.action.takes_labels() {
        head.names
            .iter()
            .map(|p| match p {
                Pattern::All => Resource::AllLabels,
                Pattern::Name(n) => Resource::Label(n.clone()),
            })
            .collect()
    } else {
        vec![Resource::Graph]
    };
    let mut out = Vec::new();
    for target in targets {
        for segment in segments {
            for resource in &resources {
                out.push(Privilege {
                    action: head.action,
                    target: Some(target.clone()),
                    segment: segment.clone(),
                    resource: resource.clone(),
                });
            }
        }
    }
    out
}
