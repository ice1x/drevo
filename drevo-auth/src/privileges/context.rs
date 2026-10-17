//! A session's [`SecurityContext`]: the privileges of the user's roles,
//! compiled once, answering each check from a cache.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::decide::{decide, Element, Request};
use super::model::{Access, Action, Level, Pattern, Privilege, Resource, Segment, Target};

/// How many distinct checks a context remembers before it starts over.
const CACHE_LIMIT: usize = 16_384;

/// What one session may see and do.
///
/// Built once from the privileges of the user's roles ([`new`](Self::new)),
/// or [`unrestricted`](Self::unrestricted) when nothing is checked (no
/// login, or in-process use). Each check is decided the first time with a
/// deny winning over any grant, and answered from a cache after that.
/// [`reads_everything`](Self::reads_everything) and
/// [`writes_everything`](Self::writes_everything) let a caller skip
/// filtering altogether for roles such as `reader` or `editor` that hold
/// graph-wide grants and no graph denies.
///
/// Cloning is cheap: clones share the compiled privileges and the cache.
///
/// ```
/// use drevo_auth::privileges::{parse, Access, Element, Op, SecurityContext};
///
/// let held: Vec<_> = [
///     "GRANT MATCH {*} ON GRAPH * TO staff",
///     "DENY READ {salary} ON GRAPH * NODES Employee TO staff",
/// ]
/// .iter()
/// .flat_map(|text| {
///     let c = parse(text).unwrap();
///     let access = if c.op == Op::Deny { Access::Deny } else { Access::Grant };
///     c.privileges.into_iter().map(move |p| (access, p))
/// })
/// .collect();
/// let context = SecurityContext::new(held, "hr");
/// let employee = Element::Node(&["Employee"]);
/// assert!(context.can_read_property("hr", employee, "name"));
/// assert!(!context.can_read_property("hr", employee, "salary"));
/// ```
#[derive(Debug, Clone)]
pub struct SecurityContext {
    compiled: Option<Arc<Compiled>>,
}

#[derive(Debug)]
struct Compiled {
    held: Vec<(Access, Privilege)>,
    home: String,
    /// Graph-wide `MATCH {*}` (or more) on every node and relationship of
    /// every graph, and no graph-level deny.
    reads_all: bool,
    /// Graph-wide `WRITE` (or more) on every graph, and no graph-level deny.
    writes_all: bool,
    cache: RwLock<HashMap<String, bool>>,
}

impl SecurityContext {
    /// A context that allows everything and checks nothing.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self { compiled: None }
    }

    /// Compile `held` — the privileges of every role the user has, granted or
    /// denied — for a user whose home database is `home`.
    #[must_use]
    pub fn new(held: impl IntoIterator<Item = (Access, Privilege)>, home: &str) -> Self {
        let held: Vec<(Access, Privilege)> = held.into_iter().collect();
        let graph_deny = held
            .iter()
            .any(|(a, p)| *a == Access::Deny && p.action.level() == Level::Graph);
        let everywhere = |p: &Privilege| matches!(p.target, None | Some(Target::All));
        let grants = |pred: &dyn Fn(&Privilege) -> bool| {
            held.iter()
                .any(|(a, p)| *a == Access::Grant && everywhere(p) && pred(p))
        };
        let reads_segment = |segment: &Segment| {
            grants(&|p| {
                (p.action == Action::AllGraph)
                    || (matches!(p.action, Action::Match | Action::Merge)
                        && p.resource == Resource::AllProperties
                        && (&p.segment == segment))
            })
        };
        let reads_all = !graph_deny
            && reads_segment(&Segment::Node(Pattern::All))
            && reads_segment(&Segment::Relationship(Pattern::All));
        let writes_all =
            !graph_deny && grants(&|p| matches!(p.action, Action::Write | Action::AllGraph));
        Self {
            compiled: Some(Arc::new(Compiled {
                held,
                home: home.to_string(),
                reads_all,
                writes_all,
                cache: RwLock::new(HashMap::new()),
            })),
        }
    }

    /// Whether this context checks nothing.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.compiled.is_none()
    }

    /// Whether every node, relationship and property of `database` can be
    /// found and read: a caller may skip read filtering.
    #[must_use]
    pub fn reads_everything(&self, database: &str) -> bool {
        match &self.compiled {
            None => true,
            Some(c) => c.reads_all && self.can_access(database),
        }
    }

    /// Whether every write to `database`'s graph is allowed: a caller may
    /// skip write checks.
    #[must_use]
    pub fn writes_everything(&self, database: &str) -> bool {
        match &self.compiled {
            None => true,
            Some(c) => c.writes_all && self.can_access(database),
        }
    }

    /// Whether the user may use `database` at all.
    #[must_use]
    pub fn can_access(&self, database: &str) -> bool {
        self.can_database(Action::Access, database)
    }

    /// Whether a node with `labels` can be found in `database`.
    #[must_use]
    pub fn can_traverse_node(&self, database: &str, labels: &[&str]) -> bool {
        self.check(Action::Traverse, database, Element::Node(labels), None)
    }

    /// Whether a relationship of `rel_type` can be found in `database`. As in
    /// Neo4j, the caller must also be able to traverse both of its nodes.
    #[must_use]
    pub fn can_traverse_relationship(&self, database: &str, rel_type: &str) -> bool {
        self.check(
            Action::Traverse,
            database,
            Element::Relationship(rel_type),
            None,
        )
    }

    /// Whether property `key` of `element` can be read; one that cannot
    /// reads as `null`.
    #[must_use]
    pub fn can_read_property(&self, database: &str, element: Element<'_>, key: &str) -> bool {
        self.check(Action::Read, database, element, Some(key))
    }

    /// Whether a graph write is allowed: `CREATE` or `DELETE` of `element`,
    /// `SET LABEL` / `REMOVE LABEL` of label `name`, or `SET PROPERTY` of
    /// property `name`.
    #[must_use]
    pub fn can_write(
        &self,
        action: Action,
        database: &str,
        element: Element<'_>,
        name: Option<&str>,
    ) -> bool {
        self.check(action, database, element, name)
    }

    /// Whether a database privilege (`ACCESS`, `INDEX MANAGEMENT`, …) is held
    /// on `database`.
    #[must_use]
    pub fn can_database(&self, action: Action, database: &str) -> bool {
        self.check(action, database, Element::None, None)
    }

    /// Whether the procedure `name` may run.
    #[must_use]
    pub fn can_execute_procedure(&self, name: &str) -> bool {
        self.check(Action::ExecuteProcedure, "", Element::None, Some(name))
    }

    /// Whether the function `name` may run.
    #[must_use]
    pub fn can_execute_function(&self, name: &str) -> bool {
        self.check(Action::ExecuteFunction, "", Element::None, Some(name))
    }

    /// Whether a DBMS privilege (`USER MANAGEMENT`, …) is held.
    #[must_use]
    pub fn has_dbms(&self, action: Action) -> bool {
        self.check(action, "", Element::None, None)
    }

    fn check(
        &self,
        action: Action,
        database: &str,
        element: Element<'_>,
        name: Option<&str>,
    ) -> bool {
        let Some(c) = &self.compiled else {
            return true;
        };
        let key = cache_key(action, database, element, name);
        if let Some(&hit) = c.cache.read().unwrap_or_else(|p| p.into_inner()).get(&key) {
            return hit;
        }
        let request = Request {
            action,
            database,
            home: &c.home,
            element,
            name,
        };
        let allowed = decide(c.held.iter().map(|(a, p)| (*a, p)), &request).is_allowed();
        let mut cache = c.cache.write().unwrap_or_else(|p| p.into_inner());
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, allowed);
        allowed
    }
}

/// A key naming one check. Labels are sorted, so their order does not
/// matter; fields are separated by a character no name holds.
fn cache_key(action: Action, database: &str, element: Element<'_>, name: Option<&str>) -> String {
    let mut key = format!("{}\u{0}{database}\u{0}", action.name());
    match element {
        Element::None => key.push('-'),
        Element::Node(labels) => {
            let mut sorted: Vec<&str> = labels.to_vec();
            sorted.sort_unstable();
            key.push('n');
            for l in sorted {
                key.push('\u{1}');
                key.push_str(l);
            }
        }
        Element::Relationship(t) => {
            key.push('r');
            key.push_str(t);
        }
    }
    key.push('\u{0}');
    if let Some(n) = name {
        key.push_str(n);
    }
    key
}
