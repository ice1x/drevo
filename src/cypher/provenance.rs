//! Where a long-term memory came from: the `provenance` map of
//! `drevo.memory.rememberEntity` / `drevo.memory.assertFact` and the chunk
//! list of `drevo.memory.addChunks`.
//!
//! A provenance map names the `:Source` a fact was read from, the `:Chunk` of
//! that source holding the passage, and the quoted words:
//!
//! ```
//! use std::collections::BTreeMap;
//! use drevo::cypher::executor::Value;
//! use drevo::cypher::provenance::{ChunkRef, Provenance, SourceRef};
//!
//! let map = BTreeMap::from([
//!     ("source".to_string(), Value::String("book-1".into())),
//!     ("chunk".to_string(), Value::String("book-1#3".into())),
//!     ("quote".to_string(), Value::String("Dana joined Acme".into())),
//! ]);
//! let p = Provenance::from_value(&Value::Map(map)).unwrap().unwrap();
//! assert_eq!(p.source, Some(SourceRef::Id("book-1".into())));
//! assert_eq!(p.chunk, Some(ChunkRef::Id("book-1#3".into())));
//! assert_eq!(p.quote.as_deref(), Some("Dana joined Acme"));
//! ```
//!
//! Nothing here touches the graph: the executor resolves the ids, upserts
//! what is given as a map and writes the edges.

use std::collections::BTreeMap;

use crate::cypher::executor::Value;

/// The keys a provenance map accepts, as listed in errors.
pub(crate) const PROVENANCE_KEYS: &str = "source, chunk, quote, offset, length, surface";

/// The keys a source map accepts, as listed in errors.
pub(crate) const SOURCE_KEYS: &str = "id, uri, title, kind";

/// The keys a chunk map accepts, as listed in errors.
pub(crate) const CHUNK_KEYS: &str = "id, order, offset, length, text";

/// How many characters of a string an error message quotes.
const PREFIX_CHARS: usize = 40;

/// A `:Source` to upsert, keyed on `id`.
///
/// ```
/// use std::collections::BTreeMap;
/// use drevo::cypher::executor::Value;
/// use drevo::cypher::provenance::SourceSpec;
///
/// let map = BTreeMap::from([
///     ("id".to_string(), Value::String("book-1".into())),
///     ("title".to_string(), Value::String("The Long Winter".into())),
/// ]);
/// let s = SourceSpec::from_value(&Value::Map(map)).unwrap();
/// assert_eq!((s.id.as_str(), s.title.as_deref(), s.uri), ("book-1", Some("The Long Winter"), None));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// The caller's id of the source.
    pub id: String,
    /// Where the source lives (a URL, a path), if known.
    pub uri: Option<String>,
    /// A human-readable title.
    pub title: Option<String>,
    /// What the source is: `document`, `url`, `message`, `file`, …
    pub kind: Option<String>,
}

/// A source named by id (it must exist) or given as a map (upserted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRef {
    /// An existing source's id.
    Id(String),
    /// A source to upsert.
    Spec(SourceSpec),
}

/// A `:Chunk` to upsert: a passage of a source, at `offset` characters into
/// it, `order`-th in reading order.
///
/// ```
/// use std::collections::BTreeMap;
/// use drevo::cypher::executor::Value;
/// use drevo::cypher::provenance::ChunkSpec;
///
/// let map = BTreeMap::from([
///     ("order".to_string(), Value::Integer(2)),
///     ("text".to_string(), Value::String("Dana joined Acme in May.".into())),
/// ]);
/// let c = ChunkSpec::from_value(&Value::Map(map)).unwrap();
/// assert_eq!((c.id, c.order, c.text.as_str()), (None, Some(2), "Dana joined Acme in May."));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSpec {
    /// The caller's id of the chunk, if given.
    pub id: Option<String>,
    /// Position in the source's reading order (gaps allowed).
    pub order: Option<i64>,
    /// Character offset of the passage in the source.
    pub offset: Option<i64>,
    /// Length of the passage in characters.
    pub length: Option<i64>,
    /// The passage.
    pub text: String,
}

/// A chunk named by id (it must exist) or given as a map (upserted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkRef {
    /// An existing chunk's id.
    Id(String),
    /// A chunk to upsert.
    Spec(ChunkSpec),
}

/// The parsed `provenance` argument.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The source the memory was read from.
    pub source: Option<SourceRef>,
    /// The chunk of the source holding the passage.
    pub chunk: Option<ChunkRef>,
    /// The quoted words; must occur in the chunk's text.
    pub quote: Option<String>,
    /// Character offset of the quote (or mention) in the chunk.
    pub offset: Option<i64>,
    /// Length of the quote (or mention) in characters.
    pub length: Option<i64>,
    /// The words naming the entity in the chunk (for a mention).
    pub surface: Option<String>,
}

impl Provenance {
    /// Parse the optional provenance argument: `null` gives `None`, a map the
    /// provenance it describes (a `null` value is the same as a missing key).
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing the accepted keys) or a value has the wrong type,
    /// is negative, or a chunk map has no `id`.
    pub fn from_value(value: &Value) -> Result<Option<Self>, String> {
        let map = match value {
            Value::Null => return Ok(None),
            Value::Map(map) => map,
            other => {
                return Err(format!(
                    "provenance must be a map {{{PROVENANCE_KEYS}}}, got {}",
                    describe(other)
                ))
            }
        };
        let mut p = Self::default();
        for (key, v) in present(map) {
            match key {
                "source" => {
                    p.source = Some(match v {
                        Value::String(id) => SourceRef::Id(id.clone()),
                        Value::Map(_) => SourceRef::Spec(SourceSpec::from_value(v)?),
                        other => {
                            return Err(format!(
                                "source must be an id or a map {{{SOURCE_KEYS}}}, got {}",
                                describe(other)
                            ))
                        }
                    });
                }
                "chunk" => {
                    p.chunk = Some(match v {
                        Value::String(id) => ChunkRef::Id(id.clone()),
                        Value::Map(_) => {
                            let spec = ChunkSpec::from_value(v)?;
                            if spec.id.is_none() {
                                return Err("a chunk map needs an id".into());
                            }
                            ChunkRef::Spec(spec)
                        }
                        other => {
                            return Err(format!(
                                "chunk must be an id or a map {{{CHUNK_KEYS}}}, got {}",
                                describe(other)
                            ))
                        }
                    });
                }
                "quote" => p.quote = Some(string(key, v)?),
                "surface" => p.surface = Some(string(key, v)?),
                "offset" => p.offset = Some(non_negative(key, v)?),
                "length" => p.length = Some(non_negative(key, v)?),
                other => {
                    return Err(format!(
                        "unknown provenance key `{other}`; expected {PROVENANCE_KEYS}"
                    ))
                }
            }
        }
        Ok(Some(p))
    }
}

impl SourceSpec {
    /// Parse a source map `{id, uri, title, kind}`; `id` is required.
    ///
    /// # Errors
    ///
    /// A message when the value is not a map, `id` is missing, a key is
    /// unknown or a value is not a string.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Map(map) = value else {
            return Err(format!(
                "a source must be a map {{{SOURCE_KEYS}}}, got {}",
                describe(value)
            ));
        };
        let mut id = None;
        let (mut uri, mut title, mut kind) = (None, None, None);
        for (key, v) in present(map) {
            match key {
                "id" => match v {
                    Value::String(s) => id = Some(s.clone()),
                    other => {
                        return Err(format!(
                            "source id must be a string, got {}",
                            describe(other)
                        ))
                    }
                },
                "uri" => uri = Some(string(key, v)?),
                "title" => title = Some(string(key, v)?),
                "kind" => kind = Some(string(key, v)?),
                other => {
                    return Err(format!(
                        "unknown source key `{other}`; expected {SOURCE_KEYS}"
                    ))
                }
            }
        }
        let id = id.ok_or_else(|| "a source map needs an id".to_string())?;
        Ok(Self {
            id,
            uri,
            title,
            kind,
        })
    }
}

impl ChunkSpec {
    /// Parse a chunk map `{id, order, offset, length, text}`; `text` is
    /// required.
    ///
    /// # Errors
    ///
    /// A message when the value is not a map, `text` is missing, a key is
    /// unknown, a value has the wrong type or a number is negative.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Map(map) = value else {
            return Err(format!(
                "a chunk must be a map {{{CHUNK_KEYS}}}, got {}",
                describe(value)
            ));
        };
        let mut id = None;
        let mut text = None;
        let (mut order, mut offset, mut length) = (None, None, None);
        for (key, v) in present(map) {
            match key {
                "id" => match v {
                    Value::String(s) => id = Some(s.clone()),
                    other => {
                        return Err(format!(
                            "chunk id must be a string, got {}",
                            describe(other)
                        ))
                    }
                },
                "order" => order = Some(non_negative(key, v)?),
                "offset" => offset = Some(non_negative(key, v)?),
                "length" => length = Some(non_negative(key, v)?),
                "text" => text = Some(string(key, v)?),
                other => {
                    return Err(format!(
                        "unknown chunk key `{other}`; expected {CHUNK_KEYS}"
                    ))
                }
            }
        }
        let text = text.ok_or_else(|| "a chunk map needs a text".to_string())?;
        Ok(Self {
            id,
            order,
            offset,
            length,
            text,
        })
    }

    /// Parse the chunk list of `drevo.memory.addChunks`: every entry needs an
    /// `order`, and an entry without an `id` gets [`default_chunk_id`].
    ///
    /// # Errors
    ///
    /// A message naming the entry (by position) when the value is not a list,
    /// an entry is not a valid chunk map or has no `order`, or two entries
    /// share an id.
    pub fn list_from_value(source_id: &str, value: &Value) -> Result<Vec<Self>, String> {
        let Value::List(items) = value else {
            return Err(format!(
                "chunks must be a list of maps {{{CHUNK_KEYS}}}, got {}",
                describe(value)
            ));
        };
        let mut chunks: Vec<Self> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let mut chunk = Self::from_value(item).map_err(|e| format!("chunk {i}: {e}"))?;
            let order = chunk
                .order
                .ok_or_else(|| format!("chunk {i} needs an order"))?;
            let id = chunk
                .id
                .take()
                .unwrap_or_else(|| default_chunk_id(source_id, order));
            if chunks.iter().any(|c| c.id.as_deref() == Some(id.as_str())) {
                return Err(format!("chunk id `{id}` is listed twice"));
            }
            chunk.id = Some(id);
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}

/// The id `drevo.memory.addChunks` gives a chunk listed without one:
/// `<sourceId>#<order>`, so adding the same list again finds the same chunks.
///
/// ```
/// use drevo::cypher::provenance::default_chunk_id;
/// assert_eq!(default_chunk_id("book-1", 3), "book-1#3");
/// ```
#[must_use]
pub fn default_chunk_id(source_id: &str, order: i64) -> String {
    format!("{source_id}#{order}")
}

/// Where `quote` occurs in `text`: the character offset of its first
/// occurrence and its length in characters, so
/// `substring(text, offset, length) = quote`; `None` when it does not occur.
///
/// ```
/// use drevo::cypher::provenance::locate_quote;
/// assert_eq!(locate_quote("Acme", "Dana joined Acme"), Some((12, 4)));
/// assert_eq!(locate_quote("Globex", "Dana joined Acme"), None);
/// ```
#[must_use]
pub fn locate_quote(quote: &str, text: &str) -> Option<(i64, i64)> {
    let byte = text.find(quote)?;
    let offset = text[..byte].chars().count();
    let length = quote.chars().count();
    Some((
        i64::try_from(offset).unwrap_or(i64::MAX),
        i64::try_from(length).unwrap_or(i64::MAX),
    ))
}

/// The error for a `what` (`quote` or `surface`) that is not in the text of
/// chunk `chunk_id`, quoting the start of both strings.
pub(crate) fn quote_not_found(what: &str, quote: &str, chunk_id: &str, text: &str) -> String {
    format!(
        "{what} '{}' is not in the text of chunk `{chunk_id}` ('{}')",
        prefix(quote),
        prefix(text)
    )
}

/// Whether `quote` stands at character `offset` of `text`: the
/// `length` characters there (the quote's own length when `None`) are the
/// quote, as `substring(text, offset, length) = quote` would check.
///
/// ```
/// use drevo::cypher::provenance::quote_at;
/// assert!(quote_at("Acme", "Dana joined Acme", 12, None));
/// assert!(!quote_at("Acme", "Dana joined Acme", 11, None));
/// ```
#[must_use]
pub fn quote_at(quote: &str, text: &str, offset: i64, length: Option<i64>) -> bool {
    let quote_len = quote.chars().count();
    let length = match length {
        Some(l) => usize::try_from(l).ok(),
        None => Some(quote_len),
    };
    let (Ok(offset), Some(length)) = (usize::try_from(offset), length) else {
        return false;
    };
    if length != quote_len || offset.saturating_add(length) > text.chars().count() {
        return false;
    }
    text.chars().skip(offset).take(length).eq(quote.chars())
}

/// The error for a `what` (`quote` or `surface`) that is not at character
/// `offset` of chunk `chunk_id`, quoting the start of both strings.
pub(crate) fn quote_not_at(
    what: &str,
    quote: &str,
    chunk_id: &str,
    offset: i64,
    text: &str,
) -> String {
    format!(
        "{what} '{}' is not at offset {offset} of chunk `{chunk_id}` ('{}')",
        prefix(quote),
        prefix(text)
    )
}

/// The first [`PREFIX_CHARS`] characters of `s`, with `…` when cut.
fn prefix(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(PREFIX_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The entries of `map` whose value is not `null` (a `null` value is the
/// same as a missing key).
fn present(map: &BTreeMap<String, Value>) -> impl Iterator<Item = (&str, &Value)> {
    map.iter()
        .filter(|(_, v)| !matches!(v, Value::Null))
        .map(|(k, v)| (k.as_str(), v))
}

/// `value` as a string, or an error naming `key`.
fn string(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Err(format!("{key} must be a string, got {}", describe(other))),
    }
}

/// `value` as an integer ≥ 0, or an error naming `key`.
fn non_negative(key: &str, value: &Value) -> Result<i64, String> {
    match value {
        Value::Integer(n) if *n >= 0 => Ok(*n),
        other => Err(format!(
            "{key} must be an integer ≥ 0, got {}",
            describe(other)
        )),
    }
}

/// A value as quoted in an error: strings in quotes, numbers as written,
/// anything else by its type.
fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => other.type_name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    // ----- provenance map ------------------------------------------------------

    #[test]
    fn null_provenance_is_none() {
        assert_eq!(Provenance::from_value(&Value::Null), Ok(None));
    }

    #[test]
    fn ids_and_scalars_are_parsed() {
        let p = Provenance::from_value(&map(&[
            ("source", s("book-1")),
            ("chunk", s("book-1#0")),
            ("quote", s("Dana")),
            ("offset", Value::Integer(4)),
            ("length", Value::Integer(4)),
            ("surface", s("Dana")),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(p.source, Some(SourceRef::Id("book-1".into())));
        assert_eq!(p.chunk, Some(ChunkRef::Id("book-1#0".into())));
        assert_eq!(p.quote.as_deref(), Some("Dana"));
        assert_eq!((p.offset, p.length), (Some(4), Some(4)));
        assert_eq!(p.surface.as_deref(), Some("Dana"));
    }

    #[test]
    fn null_values_are_missing_keys() {
        let p = Provenance::from_value(&map(&[("source", Value::Null), ("quote", Value::Null)]))
            .unwrap()
            .unwrap();
        assert_eq!(p, Provenance::default());
    }

    #[test]
    fn source_and_chunk_maps_are_specs() {
        let p = Provenance::from_value(&map(&[
            (
                "source",
                map(&[
                    ("id", s("book-1")),
                    ("uri", s("file:///b1.txt")),
                    ("title", s("Book One")),
                    ("kind", s("document")),
                ]),
            ),
            (
                "chunk",
                map(&[
                    ("id", s("c1")),
                    ("order", Value::Integer(0)),
                    ("offset", Value::Integer(0)),
                    ("length", Value::Integer(5)),
                    ("text", s("Hello")),
                ]),
            ),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            p.source,
            Some(SourceRef::Spec(SourceSpec {
                id: "book-1".into(),
                uri: Some("file:///b1.txt".into()),
                title: Some("Book One".into()),
                kind: Some("document".into()),
            }))
        );
        assert_eq!(
            p.chunk,
            Some(ChunkRef::Spec(ChunkSpec {
                id: Some("c1".into()),
                order: Some(0),
                offset: Some(0),
                length: Some(5),
                text: "Hello".into(),
            }))
        );
    }

    #[test]
    fn non_map_provenance_is_refused() {
        let err = Provenance::from_value(&s("book-1")).unwrap_err();
        assert!(err.contains("provenance must be a map"), "{err}");
    }

    #[test]
    fn unknown_provenance_key_lists_the_accepted_keys() {
        let err = Provenance::from_value(&map(&[("page", Value::Integer(3))])).unwrap_err();
        assert!(err.contains("unknown provenance key `page`"), "{err}");
        assert!(err.contains(PROVENANCE_KEYS), "{err}");
    }

    #[test]
    fn wrong_types_are_refused() {
        let err = Provenance::from_value(&map(&[("quote", Value::Integer(1))])).unwrap_err();
        assert!(err.contains("quote must be a string, got 1"), "{err}");
        let err = Provenance::from_value(&map(&[("offset", s("x"))])).unwrap_err();
        assert!(
            err.contains("offset must be an integer ≥ 0, got 'x'"),
            "{err}"
        );
        let err = Provenance::from_value(&map(&[("length", Value::Integer(-1))])).unwrap_err();
        assert!(
            err.contains("length must be an integer ≥ 0, got -1"),
            "{err}"
        );
        let err = Provenance::from_value(&map(&[("source", Value::Integer(7))])).unwrap_err();
        assert!(
            err.contains("source must be an id or a map {id, uri, title, kind}, got 7"),
            "{err}"
        );
        let err = Provenance::from_value(&map(&[("chunk", Value::Bool(true))])).unwrap_err();
        assert!(
            err.contains("chunk must be an id or a map {id, order, offset, length, text}"),
            "{err}"
        );
    }

    #[test]
    fn a_chunk_map_in_provenance_needs_an_id() {
        let err =
            Provenance::from_value(&map(&[("chunk", map(&[("text", s("Hello"))]))])).unwrap_err();
        assert!(err.contains("a chunk map needs an id"), "{err}");
    }

    // ----- source map ----------------------------------------------------------

    #[test]
    fn a_source_map_needs_a_string_id() {
        let err = SourceSpec::from_value(&map(&[("title", s("T"))])).unwrap_err();
        assert!(err.contains("a source map needs an id"), "{err}");
        let err = SourceSpec::from_value(&map(&[("id", Value::Integer(1))])).unwrap_err();
        assert!(err.contains("source id must be a string, got 1"), "{err}");
    }

    #[test]
    fn unknown_source_key_lists_the_accepted_keys() {
        let err = SourceSpec::from_value(&map(&[("id", s("b")), ("author", s("x"))])).unwrap_err();
        assert!(err.contains("unknown source key `author`"), "{err}");
        assert!(err.contains(SOURCE_KEYS), "{err}");
    }

    // ----- chunk map / list ----------------------------------------------------

    #[test]
    fn a_chunk_map_needs_text() {
        let err = ChunkSpec::from_value(&map(&[("order", Value::Integer(0))])).unwrap_err();
        assert!(err.contains("a chunk map needs a text"), "{err}");
    }

    #[test]
    fn unknown_chunk_key_lists_the_accepted_keys() {
        let err = ChunkSpec::from_value(&map(&[("text", s("t")), ("page", Value::Integer(1))]))
            .unwrap_err();
        assert!(err.contains("unknown chunk key `page`"), "{err}");
        assert!(err.contains(CHUNK_KEYS), "{err}");
    }

    #[test]
    fn negative_chunk_order_is_refused() {
        let err = ChunkSpec::from_value(&map(&[("text", s("t")), ("order", Value::Integer(-2))]))
            .unwrap_err();
        assert!(
            err.contains("order must be an integer ≥ 0, got -2"),
            "{err}"
        );
    }

    #[test]
    fn chunk_list_defaults_ids_from_order() {
        let list = Value::List(vec![
            map(&[("order", Value::Integer(0)), ("text", s("a"))]),
            map(&[
                ("id", s("x")),
                ("order", Value::Integer(5)),
                ("text", s("b")),
            ]),
        ]);
        let chunks = ChunkSpec::list_from_value("book-1", &list).unwrap();
        assert_eq!(chunks[0].id.as_deref(), Some("book-1#0"));
        assert_eq!(chunks[1].id.as_deref(), Some("x"));
        assert_eq!(chunks[1].order, Some(5));
    }

    #[test]
    fn chunk_list_entries_need_an_order() {
        let list = Value::List(vec![map(&[("text", s("a"))])]);
        let err = ChunkSpec::list_from_value("b", &list).unwrap_err();
        assert!(err.contains("chunk 0 needs an order"), "{err}");
    }

    #[test]
    fn chunk_list_errors_name_the_entry() {
        let list = Value::List(vec![
            map(&[("order", Value::Integer(0)), ("text", s("a"))]),
            map(&[("order", Value::Integer(1))]),
        ]);
        let err = ChunkSpec::list_from_value("b", &list).unwrap_err();
        assert!(err.contains("chunk 1: a chunk map needs a text"), "{err}");
    }

    #[test]
    fn chunk_list_refuses_repeated_ids() {
        let list = Value::List(vec![
            map(&[
                ("id", s("x")),
                ("order", Value::Integer(0)),
                ("text", s("a")),
            ]),
            map(&[
                ("id", s("x")),
                ("order", Value::Integer(1)),
                ("text", s("b")),
            ]),
        ]);
        let err = ChunkSpec::list_from_value("b", &list).unwrap_err();
        assert!(err.contains("chunk id `x` is listed twice"), "{err}");
    }

    #[test]
    fn chunk_list_must_be_a_list() {
        let err = ChunkSpec::list_from_value("b", &s("x")).unwrap_err();
        assert!(err.contains("chunks must be a list of maps"), "{err}");
    }

    #[test]
    fn default_chunk_id_is_source_and_order() {
        assert_eq!(default_chunk_id("book-1", 7), "book-1#7");
    }

    // ----- quotes --------------------------------------------------------------

    #[test]
    fn quote_offsets_count_characters() {
        // "é" is two bytes but one character, like Cypher's substring().
        assert_eq!(locate_quote("Acme", "Dé joined Acme"), Some((10, 4)));
        assert_eq!(locate_quote("", "abc"), Some((0, 0)));
        assert_eq!(locate_quote("x", "abc"), None);
    }

    #[test]
    fn a_quote_not_in_the_text_names_both_prefixes() {
        let text = "Dana joined Acme in May and stayed for three long, productive years.";
        let err = quote_not_found("quote", "Dana left Globex", "c1", text);
        assert_eq!(
            err,
            "quote 'Dana left Globex' is not in the text of chunk `c1` \
             ('Dana joined Acme in May and stayed for t…')"
        );
    }

    #[test]
    fn quote_at_counts_characters_not_bytes() {
        // Cyrillic letters are two bytes each: "Акме" starts at character 16
        // but at byte 29.
        let text = "Дана вступила в Акме редактором.";
        assert_eq!(text.find("Акме"), Some(29));
        assert!(quote_at("Акме", text, 16, None));
        assert!(quote_at("Акме", text, 16, Some(4)));
        assert!(!quote_at("Акме", text, 29, None));
        assert!(!quote_at("Акме", text, 15, None));
    }

    #[test]
    fn quote_at_checks_the_given_length() {
        assert!(!quote_at("Acme", "Dana joined Acme", 12, Some(3)));
        assert!(!quote_at("Acme", "Dana joined Acme", 12, Some(5)));
    }

    #[test]
    fn quote_at_past_the_end_is_false() {
        assert!(!quote_at("Acme", "Dana joined Acme", 14, None));
        assert!(!quote_at("Acme", "Dana joined Acme", 400, None));
        assert!(quote_at("", "abc", 3, None));
    }

    #[test]
    fn a_quote_not_at_its_offset_names_chunk_offset_and_both_prefixes() {
        let text = "Дана вступила в Акме редактором и проработала там много долгих лет.";
        let err = quote_not_at("quote", "Акме", "c7", 3, text);
        assert_eq!(
            err,
            "quote 'Акме' is not at offset 3 of chunk `c7` \
             ('Дана вступила в Акме редактором и прораб…')"
        );
    }

    #[test]
    fn prefixes_cut_at_forty_characters() {
        assert_eq!(prefix("short"), "short");
        assert_eq!(prefix(&"x".repeat(41)), format!("{}…", "x".repeat(40)));
    }
}
