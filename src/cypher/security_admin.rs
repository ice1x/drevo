//! Cypher user-management commands, in Neo4j 5 syntax:
//!
//! ```text
//! CREATE USER name [IF NOT EXISTS]
//!     SET [PLAINTEXT] PASSWORD password [[SET PASSWORD] CHANGE [NOT] REQUIRED]
//!     [SET STATUS {ACTIVE | SUSPENDED}]
//! ALTER USER name [IF EXISTS]
//!     { SET [PLAINTEXT] PASSWORD password [CHANGE [NOT] REQUIRED]
//!     | SET PASSWORD CHANGE [NOT] REQUIRED
//!     | SET STATUS {ACTIVE | SUSPENDED} } ...
//! DROP USER name [IF EXISTS]
//! SHOW USERS
//! SHOW CURRENT USER
//! ALTER CURRENT USER SET PASSWORD FROM password TO password
//!
//! CREATE ROLE name [IF NOT EXISTS] [AS COPY OF other]
//! DROP ROLE name [IF EXISTS]
//! RENAME ROLE name [IF EXISTS] TO new
//! GRANT ROLE[S] role[, ...] TO user[, ...]
//! REVOKE ROLE[S] role[, ...] FROM user[, ...]
//! SHOW [ALL | POPULATED] ROLES [WITH USERS]
//! GRANT | DENY privilege TO role[, ...]
//! REVOKE [GRANT | DENY] privilege FROM role[, ...]
//! SHOW [ALL | ROLE[S] role[, ...] | USER[S] [user[, ...]]] PRIVILEGE[S]
//!     [AS [REVOKE] COMMAND[S]]
//! SHOW SUPPORTED PRIVILEGE[S]
//! ```
//!
//! Privileges are read by [`drevo_auth::privileges::parse`], which describes
//! them.
//!
//! A name is a bare word or a backtick-quoted one; a password is a string
//! literal (`'…'` or `"…"`, with `\` escapes) or a `$parameter`. Keywords are
//! case-insensitive and one trailing `;` is allowed.
//!
//! Like `SHOW DATABASES` ([`crate::cypher::admin`]) these are not graph
//! queries: they are recognised here, before the graph parser, and run by the
//! server against its users. This module is syntax only.

/// Whether a user may log in, as written in `SET STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStatus {
    /// `ACTIVE`
    Active,
    /// `SUSPENDED`
    Suspended,
}

/// A password in a command: a literal, or the name of a query parameter.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    /// A string literal.
    Literal(String),
    /// A `$parameter`, by name.
    Param(String),
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
            Self::Param(name) => write!(f, "Param({name:?})"),
        }
    }
}

/// Whose privileges `SHOW … PRIVILEGES` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeScope {
    /// `SHOW [ALL] PRIVILEGES`: every role's.
    All,
    /// `SHOW ROLE r, … PRIVILEGES`.
    Roles(Vec<String>),
    /// `SHOW USER u, … PRIVILEGES`: those of the users' roles.
    Users(Vec<String>),
    /// `SHOW USER PRIVILEGES`: the signed-in user's.
    CurrentUser,
}

/// A recognised user, role or privilege command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityCommand {
    /// `CREATE USER`
    CreateUser {
        /// The new user's name.
        name: String,
        /// The password.
        password: Secret,
        /// `CHANGE [NOT] REQUIRED`, if given.
        change_required: Option<bool>,
        /// `SET STATUS`, if given.
        status: Option<UserStatus>,
        /// `IF NOT EXISTS`
        if_not_exists: bool,
    },
    /// `ALTER USER`
    AlterUser {
        /// The user to change.
        name: String,
        /// `IF EXISTS`
        if_exists: bool,
        /// A new password, if given.
        password: Option<Secret>,
        /// `CHANGE [NOT] REQUIRED`, if given.
        change_required: Option<bool>,
        /// `SET STATUS`, if given.
        status: Option<UserStatus>,
    },
    /// `DROP USER`
    DropUser {
        /// The user to drop.
        name: String,
        /// `IF EXISTS`
        if_exists: bool,
    },
    /// `SHOW USERS`
    ShowUsers,
    /// `SHOW CURRENT USER`
    ShowCurrentUser,
    /// `ALTER CURRENT USER SET PASSWORD FROM … TO …`
    AlterCurrentUserPassword {
        /// The password now.
        current: Secret,
        /// The new password.
        new: Secret,
    },
    /// `CREATE ROLE`
    CreateRole {
        /// The new role's name.
        name: String,
        /// `IF NOT EXISTS`
        if_not_exists: bool,
        /// `AS COPY OF`: the role whose privileges it starts with.
        copy_of: Option<String>,
    },
    /// `DROP ROLE`
    DropRole {
        /// The role to drop.
        name: String,
        /// `IF EXISTS`
        if_exists: bool,
    },
    /// `RENAME ROLE`
    RenameRole {
        /// The role's name now.
        from: String,
        /// Its new name.
        to: String,
        /// `IF EXISTS`
        if_exists: bool,
    },
    /// `GRANT ROLE[S] … TO …`
    GrantRoles {
        /// The roles to give.
        roles: Vec<String>,
        /// The users to give them to.
        users: Vec<String>,
    },
    /// `REVOKE ROLE[S] … FROM …`
    RevokeRoles {
        /// The roles to take.
        roles: Vec<String>,
        /// The users to take them from.
        users: Vec<String>,
    },
    /// `SHOW [ALL | POPULATED] ROLES [WITH USERS]`
    ShowRoles {
        /// `POPULATED`: only roles that some user has.
        populated: bool,
        /// `WITH USERS`: one row per role and member.
        with_users: bool,
    },
    /// `GRANT`, `DENY` or `REVOKE` of privileges.
    Privilege(drevo_auth::privileges::Command),
    /// `SHOW … PRIVILEGES`
    ShowPrivileges {
        /// Whose.
        scope: PrivilegeScope,
        /// `AS COMMANDS` (`Some(false)`) or `AS REVOKE COMMANDS`
        /// (`Some(true)`); `None` for rows.
        as_commands: Option<bool>,
    },
    /// `SHOW SUPPORTED PRIVILEGES`
    ShowSupportedPrivileges,
}

/// Classify `input`: `None` when it is not a user-management command (it goes
/// to the graph parser), `Some(Err(message))` for a malformed one, and
/// `Some(Ok(command))` otherwise. A message never contains a password.
#[must_use]
pub fn parse(input: &str) -> Option<Result<SecurityCommand, String>> {
    if is_privilege_command(input) {
        let text = input.trim().trim_end_matches(';');
        return Some(
            drevo_auth::privileges::parse(text)
                .map(SecurityCommand::Privilege)
                .map_err(|e| e.to_string()),
        );
    }
    let tokens = match tokenize(input) {
        Ok(tokens) => tokens,
        // Only report a tokenizer error for something that starts like a user
        // command; anything else is the graph parser's to explain.
        Err(message) => return starts_like_user_command(input).then_some(Err(message)),
    };
    let mut p = Parser { tokens, pos: 0 };
    let kind = p.command_kind()?;
    Some(p.command(kind))
}

#[derive(Clone, Copy)]
enum Kind {
    Create,
    Alter,
    Drop,
    ShowUsers,
    ShowCurrent,
    AlterCurrent,
    CreateRole,
    DropRole,
    RenameRole,
    GrantRoles,
    RevokeRoles,
    ShowRoles,
    ShowPrivileges,
}

/// The first words of `input`, upper-cased.
fn first_words(input: &str, n: usize) -> Vec<String> {
    input
        .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// `GRANT` / `DENY` / `REVOKE` of a privilege, as opposed to `GRANT ROLE r TO
/// u` (`GRANT ROLE MANAGEMENT …` is a privilege).
fn is_privilege_command(input: &str) -> bool {
    let words = first_words(input, 4);
    let w = |i: usize| words.get(i).map(String::as_str);
    match w(0) {
        Some("DENY") => true,
        Some("GRANT" | "REVOKE") => {
            // REVOKE GRANT / REVOKE DENY name the privilege after.
            let at = if matches!(w(1), Some("GRANT" | "DENY")) && w(0) == Some("REVOKE") {
                2
            } else {
                1
            };
            !(matches!(w(at), Some("ROLE" | "ROLES")) && w(at + 1) != Some("MANAGEMENT"))
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    Str(String),
    Param(String),
    Other(char),
}

fn starts_like_user_command(input: &str) -> bool {
    let words: Vec<String> = input
        .split_whitespace()
        .take(3)
        .map(str::to_ascii_uppercase)
        .collect();
    matches!(
        words
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["CREATE" | "ALTER" | "DROP", "USER", ..]
            | ["CREATE" | "DROP" | "RENAME", "ROLE", ..]
            | ["GRANT" | "REVOKE", "ROLE" | "ROLES", ..]
            | [
                "SHOW",
                "ROLES" | "ROLE" | "PRIVILEGES" | "PRIVILEGE" | "SUPPORTED" | "POPULATED",
                ..
            ]
            | ["SHOW", "ALL", "ROLES" | "PRIVILEGES" | "PRIVILEGE"]
            | ["SHOW", "USERS" | "USERS;", ..]
            | ["SHOW" | "ALTER", "CURRENT", "USER" | "USER;"]
            | ["CREATE", "OR", "REPLACE"]
    )
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '\'' || c == '"' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    None => return Err("unterminated string".to_string()),
                    Some('\\') => match chars.next() {
                        Some('n') => s.push('\n'),
                        Some('t') => s.push('\t'),
                        Some(other) => s.push(other),
                        None => return Err("unterminated string".to_string()),
                    },
                    Some(q) if q == c => break,
                    Some(other) => s.push(other),
                }
            }
            out.push(Token::Str(s));
        } else if c == '`' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    None => return Err("unterminated backtick name".to_string()),
                    Some('`') if chars.peek() == Some(&'`') => {
                        chars.next();
                        s.push('`');
                    }
                    Some('`') => break,
                    Some(other) => s.push(other),
                }
            }
            out.push(Token::Quoted(s));
        } else if c == '$' {
            chars.next();
            let mut s = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_alphanumeric() || n == '_' {
                    s.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Token::Param(s));
        } else if is_word_char(c) {
            let mut s = String::new();
            while let Some(&n) = chars.peek() {
                if is_word_char(n) {
                    s.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Token::Word(s));
        } else {
            chars.next();
            out.push(Token::Other(c));
        }
    }
    if out.last() == Some(&Token::Other(';')) {
        out.pop();
    }
    Ok(out)
}

/// Characters of a bare word: letters, digits and the extra characters a user
/// name may hold.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '-')
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek_word(&self, offset: usize) -> Option<String> {
        match self.tokens.get(self.pos + offset) {
            Some(Token::Word(w)) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }

    /// Which command this is, or `None` when it is not a user command.
    fn command_kind(&mut self) -> Option<Kind> {
        let first = self.peek_word(0)?;
        let second = self.peek_word(1);
        let third = self.peek_word(2);
        let mentions_privileges = self.tokens[self.pos..].iter().any(
            |t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("PRIVILEGES") || w.eq_ignore_ascii_case("PRIVILEGE")),
        );
        let (kind, len) = match (first.as_str(), second.as_deref(), third.as_deref()) {
            ("CREATE", Some("ROLE"), _) => (Kind::CreateRole, 2),
            ("CREATE", Some("OR"), Some("REPLACE"))
                if self.peek_word(3).as_deref() == Some("ROLE") =>
            {
                (Kind::CreateRole, 0)
            }
            ("DROP", Some("ROLE"), _) => (Kind::DropRole, 2),
            ("RENAME", Some("ROLE"), _) => (Kind::RenameRole, 2),
            ("GRANT", Some("ROLE" | "ROLES"), _) => (Kind::GrantRoles, 2),
            ("REVOKE", Some("ROLE" | "ROLES"), _) => (Kind::RevokeRoles, 2),
            ("SHOW", Some("USER" | "USERS" | "ROLE" | "ROLES" | "ALL"), _)
                if mentions_privileges =>
            {
                (Kind::ShowPrivileges, 1)
            }
            ("SHOW", Some("PRIVILEGES" | "PRIVILEGE" | "SUPPORTED"), _) => {
                (Kind::ShowPrivileges, 1)
            }
            ("SHOW", Some("ROLES"), _) => (Kind::ShowRoles, 1),
            ("SHOW", Some("ALL" | "POPULATED"), Some("ROLES")) => (Kind::ShowRoles, 1),
            ("CREATE", Some("USER"), _) => (Kind::Create, 2),
            ("CREATE", Some("OR"), Some("REPLACE")) => (Kind::Create, 0),
            ("ALTER", Some("USER"), _) => (Kind::Alter, 2),
            ("DROP", Some("USER"), _) => (Kind::Drop, 2),
            ("SHOW", Some("USERS"), _) => (Kind::ShowUsers, 2),
            ("SHOW", Some("CURRENT"), Some("USER")) => (Kind::ShowCurrent, 3),
            ("ALTER", Some("CURRENT"), Some("USER")) => (Kind::AlterCurrent, 3),
            _ => return None,
        };
        self.pos += len;
        Some(kind)
    }

    fn command(&mut self, kind: Kind) -> Result<SecurityCommand, String> {
        let cmd = match kind {
            Kind::Create => self.create()?,
            Kind::Alter => self.alter()?,
            Kind::Drop => {
                let name = self.name()?;
                let if_exists = self.keywords(&["IF", "EXISTS"]);
                SecurityCommand::DropUser { name, if_exists }
            }
            Kind::ShowUsers => {
                if self.keyword("YIELD") || self.keyword("WHERE") {
                    return Err("SHOW USERS does not support YIELD or WHERE yet".to_string());
                }
                SecurityCommand::ShowUsers
            }
            Kind::ShowCurrent => SecurityCommand::ShowCurrentUser,
            Kind::CreateRole => self.create_role()?,
            Kind::DropRole => {
                let name = self.role_name()?;
                let if_exists = self.keywords(&["IF", "EXISTS"]);
                SecurityCommand::DropRole { name, if_exists }
            }
            Kind::RenameRole => {
                let from = self.role_name()?;
                let if_exists = self.keywords(&["IF", "EXISTS"]);
                if !self.keyword("TO") {
                    return Err("expected TO <new name> after the role name".to_string());
                }
                let to = self.role_name()?;
                SecurityCommand::RenameRole {
                    from,
                    to,
                    if_exists,
                }
            }
            Kind::GrantRoles | Kind::RevokeRoles => {
                let roles = self.list(Self::role_name)?;
                let grant = matches!(kind, Kind::GrantRoles);
                let word = if grant { "TO" } else { "FROM" };
                if !self.keyword(word) {
                    return Err(format!("expected {word} and the users after the roles"));
                }
                let users = self.list(Self::name)?;
                if grant {
                    SecurityCommand::GrantRoles { roles, users }
                } else {
                    SecurityCommand::RevokeRoles { roles, users }
                }
            }
            Kind::ShowRoles => {
                let populated = self.keyword("POPULATED");
                if !populated {
                    self.keyword("ALL");
                }
                self.keyword("ROLES");
                let with_users =
                    self.keywords(&["WITH", "USERS"]) || self.keywords(&["WITH", "USER"]);
                if self
                    .peek_word(0)
                    .is_some_and(|w| w == "YIELD" || w == "WHERE")
                {
                    return Err("SHOW ROLES does not support YIELD or WHERE yet".to_string());
                }
                SecurityCommand::ShowRoles {
                    populated,
                    with_users,
                }
            }
            Kind::ShowPrivileges => self.show_privileges()?,
            Kind::AlterCurrent => {
                if !self.keywords(&["SET", "PASSWORD", "FROM"]) {
                    return Err(
                        "expected SET PASSWORD FROM <current> TO <new> after ALTER CURRENT USER"
                            .to_string(),
                    );
                }
                let current = self.secret()?;
                if !self.keyword("TO") {
                    return Err("expected TO <new password>".to_string());
                }
                let new = self.secret()?;
                SecurityCommand::AlterCurrentUserPassword { current, new }
            }
        };
        match self.tokens.get(self.pos) {
            None => Ok(cmd),
            Some(token) => Err(format!("unexpected {}", describe(token))),
        }
    }

    fn create_role(&mut self) -> Result<SecurityCommand, String> {
        if self.peek_word(0).as_deref() == Some("CREATE") {
            return Err("CREATE OR REPLACE ROLE is not supported; DROP the role first".to_string());
        }
        let name = self.role_name()?;
        let if_not_exists = self.keywords(&["IF", "NOT", "EXISTS"]);
        let copy_of = if self.keyword("AS") {
            if !self.keywords(&["COPY", "OF"]) {
                return Err("expected COPY OF <role> after AS".to_string());
            }
            Some(self.role_name()?)
        } else {
            None
        };
        Ok(SecurityCommand::CreateRole {
            name,
            if_not_exists,
            copy_of,
        })
    }

    /// After `SHOW`: `[ALL] PRIVILEGES`, `ROLE[S] … PRIVILEGES`, `USER[S]
    /// [ …] PRIVILEGES` or `SUPPORTED PRIVILEGES`, then `AS [REVOKE]
    /// COMMANDS`.
    fn show_privileges(&mut self) -> Result<SecurityCommand, String> {
        let privileges = |p: &mut Self| p.keyword("PRIVILEGES") || p.keyword("PRIVILEGE");
        if self.keyword("SUPPORTED") {
            if !privileges(self) {
                return Err("expected PRIVILEGES after SHOW SUPPORTED".to_string());
            }
            return Ok(SecurityCommand::ShowSupportedPrivileges);
        }
        let scope = if self.keyword("ROLE") || self.keyword("ROLES") {
            PrivilegeScope::Roles(self.list(Self::role_name)?)
        } else if self.keyword("USER") || self.keyword("USERS") {
            if matches!(
                self.peek_word(0).as_deref(),
                Some("PRIVILEGES" | "PRIVILEGE")
            ) {
                PrivilegeScope::CurrentUser
            } else {
                PrivilegeScope::Users(self.list(Self::name)?)
            }
        } else {
            self.keyword("ALL");
            PrivilegeScope::All
        };
        if !privileges(self) {
            return Err("expected PRIVILEGES".to_string());
        }
        let as_commands = if self.keyword("AS") {
            let revoke = self.keyword("REVOKE");
            if !(self.keyword("COMMANDS") || self.keyword("COMMAND")) {
                return Err("expected AS [REVOKE] COMMANDS".to_string());
            }
            Some(revoke)
        } else {
            None
        };
        if self
            .peek_word(0)
            .is_some_and(|w| w == "YIELD" || w == "WHERE")
        {
            return Err("SHOW PRIVILEGES does not support YIELD or WHERE yet".to_string());
        }
        Ok(SecurityCommand::ShowPrivileges { scope, as_commands })
    }

    /// One or more names separated by commas.
    fn list(
        &mut self,
        item: fn(&mut Self) -> Result<String, String>,
    ) -> Result<Vec<String>, String> {
        let mut out = vec![item(self)?];
        while self.tokens.get(self.pos) == Some(&Token::Other(',')) {
            self.pos += 1;
            out.push(item(self)?);
        }
        Ok(out)
    }

    fn role_name(&mut self) -> Result<String, String> {
        self.name().map_err(|_| "expected a role name".to_string())
    }

    fn create(&mut self) -> Result<SecurityCommand, String> {
        if self.peek_word(0).as_deref() == Some("CREATE") {
            return Err("CREATE OR REPLACE USER is not supported; DROP the user first".to_string());
        }
        let name = self.name()?;
        let if_not_exists = self.keywords(&["IF", "NOT", "EXISTS"]);
        let password = match self.set_password()? {
            Some(PasswordClause::Value(secret)) => secret,
            _ => return Err("expected SET PASSWORD <password> after the user name".to_string()),
        };
        let mut change_required = self.change_required()?;
        let mut status = None;
        while self.keyword("SET") {
            if self.keyword("PASSWORD") {
                change_required = Some(self.required_change()?);
            } else if self.keyword("STATUS") {
                status = Some(self.status()?);
            } else {
                return Err(self.unsupported_set());
            }
        }
        Ok(SecurityCommand::CreateUser {
            name,
            password,
            change_required,
            status,
            if_not_exists,
        })
    }

    fn alter(&mut self) -> Result<SecurityCommand, String> {
        let name = self.name()?;
        let if_exists = self.keywords(&["IF", "EXISTS"]);
        let (mut password, mut change_required, mut status) = (None, None, None);
        let mut any = false;
        loop {
            match self.set_password()? {
                Some(PasswordClause::Value(secret)) => {
                    if password.replace(secret).is_some() {
                        return Err("PASSWORD is set twice".to_string());
                    }
                    if let Some(required) = self.change_required()? {
                        change_required = Some(required);
                    }
                }
                Some(PasswordClause::ChangeRequired(required)) => change_required = Some(required),
                None if self.keywords(&["SET", "STATUS"]) => {
                    if status.replace(self.status()?).is_some() {
                        return Err("STATUS is set twice".to_string());
                    }
                }
                None if self.peek_word(0).as_deref() == Some("SET") => {
                    self.pos += 1;
                    return Err(self.unsupported_set());
                }
                None => break,
            }
            any = true;
        }
        if !any {
            return Err("expected SET PASSWORD or SET STATUS after the user name".to_string());
        }
        Ok(SecurityCommand::AlterUser {
            name,
            if_exists,
            password,
            change_required,
            status,
        })
    }

    /// `SET [PLAINTEXT] PASSWORD <secret>` or `SET PASSWORD CHANGE [NOT]
    /// REQUIRED`; `None` (consuming nothing) when the next words are not that.
    fn set_password(&mut self) -> Result<Option<PasswordClause>, String> {
        let start = self.pos;
        if !self.keyword("SET") {
            return Ok(None);
        }
        if self.keyword("ENCRYPTED") {
            return Err(
                "ENCRYPTED passwords are not supported; give the password itself".to_string(),
            );
        }
        self.keyword("PLAINTEXT");
        if !self.keyword("PASSWORD") {
            self.pos = start;
            return Ok(None);
        }
        if self.peek_word(0).as_deref() == Some("CHANGE") {
            return Ok(Some(PasswordClause::ChangeRequired(
                self.required_change()?,
            )));
        }
        Ok(Some(PasswordClause::Value(self.secret()?)))
    }

    /// An optional `CHANGE [NOT] REQUIRED`.
    fn change_required(&mut self) -> Result<Option<bool>, String> {
        if self.peek_word(0).as_deref() == Some("CHANGE") {
            self.required_change().map(Some)
        } else {
            Ok(None)
        }
    }

    /// `CHANGE [NOT] REQUIRED`: `true` for required.
    fn required_change(&mut self) -> Result<bool, String> {
        if !self.keyword("CHANGE") {
            return Err("expected CHANGE [NOT] REQUIRED".to_string());
        }
        let not = self.keyword("NOT");
        if !self.keyword("REQUIRED") {
            return Err("expected CHANGE [NOT] REQUIRED".to_string());
        }
        Ok(!not)
    }

    fn status(&mut self) -> Result<UserStatus, String> {
        if self.keyword("ACTIVE") {
            Ok(UserStatus::Active)
        } else if self.keyword("SUSPENDED") {
            Ok(UserStatus::Suspended)
        } else {
            Err("expected ACTIVE or SUSPENDED after SET STATUS".to_string())
        }
    }

    /// The error for a `SET` that is followed by something unsupported. The
    /// `SET` itself has been consumed.
    fn unsupported_set(&self) -> String {
        match self.tokens.get(self.pos) {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("HOME") => {
                "SET HOME DATABASE is not supported".to_string()
            }
            Some(Token::Word(w)) => {
                format!("unknown SET {w}; expected SET PASSWORD or SET STATUS")
            }
            Some(token) => format!("unexpected {} after SET", describe(token)),
            None => "expected PASSWORD or STATUS after SET".to_string(),
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.tokens.get(self.pos) {
            Some(Token::Word(w)) if !is_clause_keyword(w) => {
                self.pos += 1;
                Ok(w.clone())
            }
            Some(Token::Quoted(q)) => {
                self.pos += 1;
                Ok(q.clone())
            }
            _ => Err("expected a user name".to_string()),
        }
    }

    fn secret(&mut self) -> Result<Secret, String> {
        match self.tokens.get(self.pos) {
            Some(Token::Str(s)) => {
                self.pos += 1;
                Ok(Secret::Literal(s.clone()))
            }
            Some(Token::Param(p)) if !p.is_empty() => {
                self.pos += 1;
                Ok(Secret::Param(p.clone()))
            }
            _ => Err("expected a password: a string literal or a $parameter".to_string()),
        }
    }

    fn keyword(&mut self, word: &str) -> bool {
        if self.peek_word(0).as_deref() == Some(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume `words` in order, or nothing.
    fn keywords(&mut self, words: &[&str]) -> bool {
        let all = words
            .iter()
            .enumerate()
            .all(|(i, w)| self.peek_word(i).as_deref() == Some(*w));
        if all {
            self.pos += words.len();
        }
        all
    }
}

enum PasswordClause {
    Value(Secret),
    ChangeRequired(bool),
}

/// Words that start a clause and so cannot be a bare user name.
fn is_clause_keyword(w: &str) -> bool {
    matches!(
        w.to_ascii_uppercase().as_str(),
        "SET"
            | "IF"
            | "FROM"
            | "TO"
            | "YIELD"
            | "WHERE"
            | "AS"
            | "WITH"
            | "PRIVILEGES"
            | "PRIVILEGE"
    )
}

/// A token for an error message. A string literal is never shown: it may be a
/// password.
fn describe(token: &Token) -> String {
    match token {
        Token::Word(w) => format!("'{w}'"),
        Token::Quoted(q) => format!("`{q}`"),
        Token::Str(_) => "string literal".to_string(),
        Token::Param(p) => format!("${p}"),
        Token::Other(c) => format!("'{c}'"),
    }
}
