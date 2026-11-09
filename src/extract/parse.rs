//! Reading a model's extraction answer.
//!
//! The model is asked for one JSON object, `{"entities": [...], "facts":
//! [...]}`. [`parse_response`] reads it — tolerating a markdown code fence or
//! words around the object — and [`accept`] keeps the items that hold up:
//! each surface or quote is located in the text (see
//! [`crate::extract::offsets::verify_or_repair`]), types are
//! upper-cased and must be allowed, relations are upper-snake-cased and
//! must be allowed when a list is given, and items below the minimum
//! confidence are dropped. Every dropped item is counted.
//!
//! ```
//! use drevo::extract::parse::{accept, parse_response, AcceptOptions};
//!
//! let text = "Dana joined Acme in May.";
//! let answer = r#"{"entities": [
//!     {"name": "Dana", "type": "person", "surface": "Dana", "offset": 0},
//!     {"name": "Globex", "type": "ORGANIZATION", "surface": "Globex", "offset": 3}
//!   ],
//!   "facts": [{"subject": "Dana", "subjectType": "PERSON", "relation": "works at",
//!              "object": "Acme", "objectType": "ORGANIZATION",
//!              "quote": "Dana joined Acme", "offset": 0, "confidence": 0.9}]}"#;
//! let types = vec!["PERSON".to_string(), "ORGANIZATION".to_string()];
//! let options = AcceptOptions { types: &types, relations: None, min_confidence: 0.5 };
//! let kept = accept(&parse_response(answer).unwrap(), text, &options);
//! assert_eq!(kept.entities.len(), 1);
//! assert_eq!(kept.entities[0].entity_type, "PERSON");
//! assert_eq!(kept.facts[0].relation, "WORKS_AT");
//! assert_eq!(kept.skipped, 1); // Globex is not in the text
//! ```

use super::offsets::verify_or_repair;

/// The two lists of a model's answer, each item still raw JSON.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawResponse {
    /// The `entities` items.
    pub entities: Vec<serde_json::Value>,
    /// The `facts` items.
    pub facts: Vec<serde_json::Value>,
}

/// What [`accept`] keeps an item against.
#[derive(Debug, Clone, Copy)]
pub struct AcceptOptions<'a> {
    /// The allowed entity types, upper-case.
    pub types: &'a [String],
    /// The allowed relations, upper-snake-case; `None` allows any.
    pub relations: Option<&'a [String]>,
    /// Items with a lower confidence are dropped.
    pub min_confidence: f64,
}

/// An entity mention found in the text.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    /// The entity's name as the model normalised it ("Dana Scully").
    pub name: String,
    /// Its type, upper-case (`PERSON`).
    pub entity_type: String,
    /// A short description, when the model gave one.
    pub description: Option<String>,
    /// Character offset of the mention in the text.
    pub offset: usize,
    /// Length of the mention in characters.
    pub length: usize,
    /// The words of the text naming it.
    pub surface: String,
    /// The model's confidence, 1.0 when it gave none.
    pub confidence: f64,
}

/// A fact found in the text, with the words that state it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// The subject entity's name.
    pub subject: String,
    /// The subject's type, upper-case.
    pub subject_type: String,
    /// The relation, upper-snake-case (`WORKS_AT`).
    pub relation: String,
    /// The object entity's name.
    pub object: String,
    /// The object's type, upper-case.
    pub object_type: String,
    /// The words of the text stating the fact.
    pub quote: String,
    /// Character offset of the quote in the text.
    pub offset: usize,
    /// Length of the quote in characters.
    pub length: usize,
    /// The model's confidence, 1.0 when it gave none.
    pub confidence: f64,
}

/// What [`accept`] kept, and how many items it dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Accepted {
    /// The entity mentions that were located in the text.
    pub entities: Vec<Entity>,
    /// The facts whose quote was located in the text.
    pub facts: Vec<Fact>,
    /// Items dropped: unlocatable, malformed, of a type or relation not
    /// allowed, or below the minimum confidence.
    pub skipped: usize,
}

/// Read a model's answer: the JSON object in `content` (inside a code fence
/// or among other words), with `entities` and `facts` lists (a missing list
/// is empty).
///
/// # Errors
///
/// A message when `content` holds no JSON object or a list is not a list.
pub fn parse_response(content: &str) -> Result<RawResponse, String> {
    let object = json_object(content).ok_or_else(|| {
        format!(
            "no JSON object in the answer ('{}')",
            prefix(content.trim())
        )
    })?;
    let list = |key: &str| match object.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(format!("`{key}` must be a list")),
    };
    Ok(RawResponse {
        entities: list("entities")?,
        facts: list("facts")?,
    })
}

/// The JSON object in `content`: all of it, else the span from its first
/// `{` to its last `}`.
fn json_object(content: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let trimmed = content.trim();
    let parse = |s: &str| match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    };
    parse(trimmed).or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        (start < end)
            .then(|| parse(&trimmed[start..=end]))
            .flatten()
    })
}

/// The first 80 characters of `s`, with `…` when cut.
fn prefix(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Keep the items of `raw` that hold up against `text` (see the module
/// docs), with offsets into `text`.
#[must_use]
pub fn accept(raw: &RawResponse, text: &str, options: &AcceptOptions<'_>) -> Accepted {
    let mut out = Accepted::default();
    for item in &raw.entities {
        match entity(item, text, options) {
            Some(e) => out.entities.push(e),
            None => out.skipped += 1,
        }
    }
    for item in &raw.facts {
        match fact(item, text, options, &out.entities) {
            Some(f) => out.facts.push(f),
            None => out.skipped += 1,
        }
    }
    out
}

/// A string field, trimmed; `None` when absent, not a string or blank.
fn text_field(item: &serde_json::Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The claimed offset: a whole number ≥ 0, else `None` (found by search).
fn offset_field(item: &serde_json::Value) -> Option<usize> {
    item.get("offset")
        .and_then(serde_json::Value::as_u64)
        .and_then(|o| usize::try_from(o).ok())
}

/// The confidence, 1.0 when absent; `None` when it is not a number.
fn confidence_field(item: &serde_json::Value) -> Option<f64> {
    match item.get("confidence") {
        None | Some(serde_json::Value::Null) => Some(1.0),
        Some(v) => v.as_f64(),
    }
}

/// An allowed type in upper snake case.
fn allowed_type(raw: &str, options: &AcceptOptions<'_>) -> Option<String> {
    let t = upper_snake(raw);
    options.types.contains(&t).then_some(t)
}

fn entity(item: &serde_json::Value, text: &str, options: &AcceptOptions<'_>) -> Option<Entity> {
    let name = text_field(item, "name")?;
    let entity_type = allowed_type(&text_field(item, "type")?, options)?;
    let confidence = confidence_field(item)?;
    if confidence < options.min_confidence {
        return None;
    }
    let surface = item
        .get("surface")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| name.clone(), str::to_string);
    let found = verify_or_repair(text, &surface, offset_field(item))?;
    Some(Entity {
        name,
        entity_type,
        description: text_field(item, "description"),
        offset: found.offset,
        length: found.length,
        surface: found.surface,
        confidence,
    })
}

fn fact(
    item: &serde_json::Value,
    text: &str,
    options: &AcceptOptions<'_>,
    entities: &[Entity],
) -> Option<Fact> {
    let subject = text_field(item, "subject")?;
    let object = text_field(item, "object")?;
    let relation = upper_snake(&text_field(item, "relation")?);
    if relation.is_empty() {
        return None;
    }
    if let Some(allowed) = options.relations {
        if !allowed.contains(&relation) {
            return None;
        }
    }
    let type_of = |key: &str, name: &str| match text_field(item, key) {
        Some(t) => allowed_type(&t, options),
        None => entities
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.entity_type.clone()),
    };
    let subject_type = type_of("subjectType", &subject)?;
    let object_type = type_of("objectType", &object)?;
    let confidence = confidence_field(item)?;
    if confidence < options.min_confidence {
        return None;
    }
    let quote = item.get("quote").and_then(serde_json::Value::as_str)?;
    let found = verify_or_repair(text, quote, offset_field(item))?;
    Some(Fact {
        subject,
        subject_type,
        relation,
        object,
        object_type,
        quote: found.surface,
        offset: found.offset,
        length: found.length,
        confidence,
    })
}

/// `name` in upper snake case, as relations and types are stored:
/// `works at`, `worksAt` and `works-at` all give `WORKS_AT`.
///
/// ```
/// use drevo::extract::parse::upper_snake;
/// assert_eq!(upper_snake("worksAt"), "WORKS_AT");
/// assert_eq!(upper_snake(" born in "), "BORN_IN");
/// ```
#[must_use]
pub fn upper_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_lower = false;
    let mut gap = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if (gap || (prev_lower && c.is_uppercase())) && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.extend(c.to_uppercase());
            prev_lower = c.is_lowercase() || c.is_numeric();
        } else {
            gap = true;
            prev_lower = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "Dana joined Acme in May. Acme is in Oslo.";

    fn types() -> Vec<String> {
        ["PERSON", "OBJECT", "LOCATION", "EVENT", "ORGANIZATION"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn run(answer: &str, relations: Option<&[String]>, min: f64) -> Accepted {
        let types = types();
        let options = AcceptOptions {
            types: &types,
            relations,
            min_confidence: min,
        };
        accept(&parse_response(answer).unwrap(), TEXT, &options)
    }

    #[test]
    fn the_contract_is_parsed() {
        let raw = parse_response(r#"{"entities": [{"name": "Dana"}], "facts": []}"#).unwrap();
        assert_eq!(raw.entities.len(), 1);
        assert!(raw.facts.is_empty());
    }

    #[test]
    fn a_code_fence_and_surrounding_words_are_tolerated() {
        let raw = parse_response("Sure!\n```json\n{\"entities\": [], \"facts\": [{}]}\n```\nDone.")
            .unwrap();
        assert_eq!(raw.facts.len(), 1);
    }

    #[test]
    fn a_missing_list_is_empty() {
        let raw = parse_response(r#"{"entities": [{"name": "Dana"}]}"#).unwrap();
        assert!(raw.facts.is_empty());
    }

    #[test]
    fn text_without_json_is_refused() {
        let err = parse_response("I could not find any entities.").unwrap_err();
        assert!(err.contains("no JSON object"), "{err}");
        let err = parse_response("{\"entities\": [").unwrap_err();
        assert!(err.contains("no JSON object"), "{err}");
    }

    #[test]
    fn a_list_that_is_not_a_list_is_refused() {
        let err = parse_response(r#"{"entities": "Dana"}"#).unwrap_err();
        assert!(err.contains("`entities` must be a list"), "{err}");
        let err = parse_response(r#"[1, 2]"#).unwrap_err();
        assert!(err.contains("no JSON object"), "{err}");
    }

    #[test]
    fn verified_entities_are_kept_and_extra_fields_ignored() {
        let kept = run(
            r#"{"entities": [{"name": "Dana", "type": "person", "surface": "Dana",
                "offset": 0, "description": "an editor", "confidence": 0.8,
                "salience": "high"}]}"#,
            None,
            0.5,
        );
        assert_eq!(kept.skipped, 0);
        let e = &kept.entities[0];
        assert_eq!(
            (e.name.as_str(), e.entity_type.as_str(), e.offset, e.length),
            ("Dana", "PERSON", 0, 4)
        );
        assert_eq!(e.description.as_deref(), Some("an editor"));
        assert!((e.confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_surface_defaults_to_the_name_and_confidence_to_one() {
        let kept = run(
            r#"{"entities": [{"name": "Oslo", "type": "LOCATION", "offset": 36}]}"#,
            None,
            0.5,
        );
        assert_eq!(kept.entities[0].surface, "Oslo");
        assert_eq!(kept.entities[0].offset, 36);
        assert!((kept.entities[0].confidence - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_hallucinated_surface_is_dropped_and_counted() {
        let kept = run(
            r#"{"entities": [{"name": "Globex", "type": "ORGANIZATION", "surface": "Globex", "offset": 3}],
                "facts": [{"subject": "Dana", "subjectType": "PERSON", "relation": "LEFT",
                           "object": "Acme", "objectType": "ORGANIZATION",
                           "quote": "Dana left Acme", "offset": 0}]}"#,
            None,
            0.5,
        );
        assert!(kept.entities.is_empty() && kept.facts.is_empty());
        assert_eq!(kept.skipped, 2);
    }

    #[test]
    fn min_confidence_drops_weak_items() {
        let kept = run(
            r#"{"entities": [{"name": "Dana", "type": "PERSON", "offset": 0, "confidence": 0.3},
                             {"name": "Acme", "type": "ORGANIZATION", "offset": 12, "confidence": 0.5}]}"#,
            None,
            0.5,
        );
        assert_eq!(kept.entities.len(), 1);
        assert_eq!(kept.entities[0].name, "Acme");
        assert_eq!(kept.skipped, 1);
    }

    #[test]
    fn a_type_that_is_not_allowed_is_dropped() {
        let kept = run(
            r#"{"entities": [{"name": "May", "type": "month", "offset": 20}]}"#,
            None,
            0.5,
        );
        assert!(kept.entities.is_empty());
        assert_eq!(kept.skipped, 1);
    }

    #[test]
    fn malformed_items_are_dropped_not_fatal() {
        let kept = run(
            r#"{"entities": [42, {"type": "PERSON"}, {"name": "Dana", "type": "PERSON", "offset": "zero"}],
                "facts": [{"subject": "Dana"}]}"#,
            None,
            0.5,
        );
        // The third entity has an unusable offset: it is located by search.
        assert_eq!(kept.entities.len(), 1);
        assert_eq!(kept.entities[0].offset, 0);
        assert_eq!(kept.skipped, 3);
    }

    #[test]
    fn relations_are_upper_snake_cased_and_checked_against_the_list() {
        let answer = r#"{"facts": [
            {"subject": "Dana", "subjectType": "PERSON", "relation": "works at",
             "object": "Acme", "objectType": "ORGANIZATION", "quote": "Dana joined Acme", "offset": 0},
            {"subject": "Acme", "subjectType": "ORGANIZATION", "relation": "locatedIn",
             "object": "Oslo", "objectType": "LOCATION", "quote": "Acme is in Oslo", "offset": 25}]}"#;
        let kept = run(answer, None, 0.5);
        let relations: Vec<&str> = kept.facts.iter().map(|f| f.relation.as_str()).collect();
        assert_eq!(relations, ["WORKS_AT", "LOCATED_IN"]);
        assert_eq!((kept.facts[1].offset, kept.facts[1].length), (25, 15));

        let allowed = vec!["LOCATED_IN".to_string()];
        let kept = run(answer, Some(&allowed), 0.5);
        assert_eq!(kept.facts.len(), 1);
        assert_eq!(kept.facts[0].relation, "LOCATED_IN");
        assert_eq!(kept.skipped, 1);
    }

    #[test]
    fn a_fact_takes_a_missing_type_from_the_entities() {
        let kept = run(
            r#"{"entities": [{"name": "Dana", "type": "PERSON", "offset": 0},
                             {"name": "Acme", "type": "ORGANIZATION", "offset": 12}],
                "facts": [{"subject": "Dana", "relation": "JOINED", "object": "Acme",
                           "quote": "Dana joined Acme", "offset": 0}]}"#,
            None,
            0.5,
        );
        assert_eq!(kept.facts.len(), 1);
        assert_eq!(kept.facts[0].subject_type, "PERSON");
        assert_eq!(kept.facts[0].object_type, "ORGANIZATION");
    }

    #[test]
    fn a_fact_without_a_known_type_or_quote_is_dropped() {
        let kept = run(
            r#"{"facts": [{"subject": "Dana", "relation": "JOINED", "object": "Acme",
                           "quote": "Dana joined Acme", "offset": 0},
                          {"subject": "Dana", "subjectType": "PERSON", "relation": "JOINED",
                           "object": "Acme", "objectType": "ORGANIZATION"}]}"#,
            None,
            0.5,
        );
        assert!(kept.facts.is_empty());
        assert_eq!(kept.skipped, 2);
    }

    #[test]
    fn upper_snake_handles_spaces_camel_case_and_unicode() {
        assert_eq!(upper_snake("works at"), "WORKS_AT");
        assert_eq!(upper_snake("worksAt"), "WORKS_AT");
        assert_eq!(upper_snake("works-at"), "WORKS_AT");
        assert_eq!(upper_snake("WORKS_AT"), "WORKS_AT");
        assert_eq!(upper_snake("__born  in__"), "BORN_IN");
        assert_eq!(upper_snake("работает в"), "РАБОТАЕТ_В");
        assert_eq!(upper_snake("HTTPServer"), "HTTPSERVER");
    }
}
