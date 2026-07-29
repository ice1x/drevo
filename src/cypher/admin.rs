//! Catalog-level Cypher admin commands — `SHOW DATABASES`, `USE`, and
//! `CREATE DATABASE`.
//!
//! These are *not* graph queries: they list, select, and create the named
//! databases the process serves (see [`crate::catalog`]). The graph executor
//! operates on a single database and knows nothing about the catalog, so
//! these commands are recognised here, at the string level, and handled by
//! the catalog-aware HTTP layer *before* the query reaches the parser.
//!
//! This module is pure syntax — it classifies a query string and extracts
//! the operands. Name validation and the actual create/list/select happen in
//! the caller against the live [`crate::catalog::Catalog`].
//!
//! Grammar (keywords case-insensitive, a single optional trailing `;`):
//!
//! ```text
//! SHOW DATABASES
//! CREATE DATABASE <name> [IF NOT EXISTS]
//! USE <name>                 -- select only
//! USE <name> <query>         -- run <query> against <name>
//! ```
//!
//! Anything else returns [`None`] and flows to the normal Cypher parser.

/// A recognised catalog admin command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminCommand {
    /// `SHOW DATABASES` — list every database in the catalog.
    ShowDatabases,
    /// `CREATE DATABASE <name> [IF NOT EXISTS]`.
    CreateDatabase {
        /// Requested database name (validated by the catalog, not here).
        name: String,
        /// `IF NOT EXISTS` present — a pre-existing name is a no-op, not an
        /// error.
        if_not_exists: bool,
    },
    /// `USE <name>` optionally followed by a query to run against it. With
    /// no trailing query the command just names a database to select.
    Use {
        /// Target database name.
        name: String,
        /// The query to run against `name`, or `None` for a bare `USE`.
        query: Option<String>,
    },
}

/// Classify `input` as a catalog admin command, or `None` if it is an
/// ordinary graph query.
#[must_use]
pub fn parse(input: &str) -> Option<AdminCommand> {
    let s = input.trim();
    let mut words = s.split_whitespace();
    let keyword = words.next()?.to_ascii_uppercase();
    match keyword.as_str() {
        "SHOW" => {
            let second = words.next()?.trim_end_matches(';');
            // Exactly `SHOW DATABASES` (nothing after it).
            if second.eq_ignore_ascii_case("DATABASES") && words.next().is_none() {
                Some(AdminCommand::ShowDatabases)
            } else {
                None
            }
        }
        "CREATE" => {
            if !words.next()?.eq_ignore_ascii_case("DATABASE") {
                return None; // e.g. `CREATE (n) …` — a real graph write.
            }
            let name = words.next()?.trim_end_matches(';').to_string();
            // Only a trailing `IF NOT EXISTS` (or nothing) is accepted; any
            // other tail flows to the parser to fail with a clear error.
            let tail: Vec<String> = words
                .map(|w| w.trim_end_matches(';').to_ascii_uppercase())
                .collect();
            let if_not_exists = match tail.as_slice() {
                [] => false,
                [a, b, c] if a == "IF" && b == "NOT" && c == "EXISTS" => true,
                _ => return None,
            };
            Some(AdminCommand::CreateDatabase {
                name,
                if_not_exists,
            })
        }
        "USE" => {
            // Take the substring after the `USE` keyword so the trailing
            // query keeps its original spacing.
            let after = s.get(keyword.len()..).unwrap_or("").trim_start();
            if after.is_empty() {
                return None;
            }
            let name_end = after.find(char::is_whitespace).unwrap_or(after.len());
            let name = after[..name_end].trim_end_matches(';').to_string();
            if name.is_empty() {
                return None;
            }
            let rest = after[name_end..].trim().trim_end_matches(';').trim();
            let query = if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
            Some(AdminCommand::Use { name, query })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_databases_any_case_and_semicolon() {
        assert_eq!(parse("SHOW DATABASES"), Some(AdminCommand::ShowDatabases));
        assert_eq!(parse("show databases"), Some(AdminCommand::ShowDatabases));
        assert_eq!(
            parse("  Show   Databases  "),
            Some(AdminCommand::ShowDatabases)
        );
        assert_eq!(parse("SHOW DATABASES;"), Some(AdminCommand::ShowDatabases));
    }

    #[test]
    fn show_other_is_not_admin() {
        assert_eq!(parse("SHOW INDEXES"), None);
        assert_eq!(parse("SHOW DATABASES foo"), None);
        assert_eq!(parse("SHOW"), None);
    }

    #[test]
    fn create_database_plain_and_if_not_exists() {
        assert_eq!(
            parse("CREATE DATABASE foo"),
            Some(AdminCommand::CreateDatabase {
                name: "foo".into(),
                if_not_exists: false
            })
        );
        assert_eq!(
            parse("create database Bar_2"),
            Some(AdminCommand::CreateDatabase {
                name: "Bar_2".into(),
                if_not_exists: false
            })
        );
        assert_eq!(
            parse("CREATE DATABASE foo IF NOT EXISTS"),
            Some(AdminCommand::CreateDatabase {
                name: "foo".into(),
                if_not_exists: true
            })
        );
        assert_eq!(
            parse("CREATE DATABASE foo;"),
            Some(AdminCommand::CreateDatabase {
                name: "foo".into(),
                if_not_exists: false
            })
        );
    }

    #[test]
    fn create_node_is_not_a_database_command() {
        assert_eq!(parse("CREATE (n) RETURN n"), None);
        assert_eq!(parse("CREATE (n:Database) RETURN n"), None);
        assert_eq!(parse("CREATE DATABASE"), None); // missing name
        assert_eq!(parse("CREATE DATABASE foo bar"), None); // unknown tail
    }

    #[test]
    fn use_selects_and_optionally_runs() {
        assert_eq!(
            parse("USE foo"),
            Some(AdminCommand::Use {
                name: "foo".into(),
                query: None
            })
        );
        assert_eq!(
            parse("USE foo;"),
            Some(AdminCommand::Use {
                name: "foo".into(),
                query: None
            })
        );
        assert_eq!(
            parse("USE foo MATCH (n) RETURN n"),
            Some(AdminCommand::Use {
                name: "foo".into(),
                query: Some("MATCH (n) RETURN n".into())
            })
        );
        assert_eq!(
            parse("use my-db RETURN 1"),
            Some(AdminCommand::Use {
                name: "my-db".into(),
                query: Some("RETURN 1".into())
            })
        );
    }

    #[test]
    fn ordinary_queries_pass_through() {
        assert_eq!(parse("MATCH (n) RETURN n"), None);
        assert_eq!(parse("RETURN 1"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        // `USE` is only special as the leading keyword.
        assert_eq!(parse("MATCH (n) WHERE n.use = 1 RETURN n"), None);
    }
}
