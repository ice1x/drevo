//! Entity resolution: finding nodes that name the same thing under different
//! spellings, and folding them into one.
//!
//! - [`normalise`] puts a name in the form names are compared in
//!   («А. С. Пушкин» → «а с пушкин»).
//! - [`blocking`] picks the pairs worth comparing.
//! - [`score`] rates a pair: Jaro–Winkler on the compared names, cosine on
//!   embeddings, and the reasons a reviewer reads.
//! - [`merge`] plans folding dropped nodes into a kept one: relationships
//!   re-attached without duplicates, properties combined, aliases recorded.
//! - [`config`] parses the config maps of `drevo.entity.candidates`,
//!   `drevo.entity.merge` and the resolution keys of
//!   `drevo.memory.rememberEntity`.
//!
//! Nothing here touches the graph: the executor reads the nodes, hands them
//! over, and writes the plan back through the regular write path.

pub(crate) mod blocking;
pub(crate) mod config;
pub(crate) mod merge;
pub(crate) mod normalise;
pub(crate) mod score;
