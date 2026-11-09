//! The instructions sent to the model for an extraction.
//!
//! [`system_prompt`] tells the model which entity types and relations to
//! look for, adds the caller's domain vocabulary and language hint, and
//! fixes the answer to one JSON object whose every item quotes the text
//! verbatim with its character offset:
//!
//! ```
//! use drevo::extract::prompt::{system_prompt, PromptOptions};
//!
//! let types = vec!["PERSON".to_string(), "ORGANIZATION".to_string()];
//! let prompt = system_prompt(&PromptOptions {
//!     types: &types,
//!     relations: None,
//!     schema_hint: Some("A novel about publishing."),
//!     language: "auto",
//!     entities: true,
//!     facts: true,
//! });
//! assert!(prompt.contains("PERSON, ORGANIZATION"));
//! assert!(prompt.contains("A novel about publishing."));
//! ```

/// What [`system_prompt`] asks for.
#[derive(Debug, Clone, Copy)]
pub struct PromptOptions<'a> {
    /// The entity types to look for, upper-case.
    pub types: &'a [String],
    /// The relations facts may use; `None` lets the model name them.
    pub relations: Option<&'a [String]>,
    /// Domain vocabulary appended to the instructions.
    pub schema_hint: Option<&'a str>,
    /// The text's language, or `auto`.
    pub language: &'a str,
    /// Whether to ask for entity mentions.
    pub entities: bool,
    /// Whether to ask for facts.
    pub facts: bool,
}

/// The follow-up sent once when an answer was not the JSON object asked for.
pub const JSON_NUDGE: &str = "Your answer was not valid JSON. Return ONLY the JSON object \
     {\"entities\": [...], \"facts\": [...]} described above — no prose, no markdown.";

/// The system prompt for an extraction (see the module docs).
#[must_use]
pub fn system_prompt(options: &PromptOptions<'_>) -> String {
    let mut p = String::from(
        "You extract a knowledge graph from a text: the entities it mentions and the facts \
         it states about them.\n\
         Answer with ONLY one JSON object, no prose and no markdown, of this shape:\n\
         {\"entities\": [{\"name\": \"Dana Scully\", \"type\": \"PERSON\", \"description\": \"an FBI agent\", \
         \"surface\": \"Dana\", \"offset\": 0, \"confidence\": 0.9}],\n \
         \"facts\": [{\"subject\": \"Dana Scully\", \"subjectType\": \"PERSON\", \"relation\": \"WORKS_AT\", \
         \"object\": \"FBI\", \"objectType\": \"ORGANIZATION\", \"quote\": \"Dana joined the FBI\", \
         \"offset\": 0, \"confidence\": 0.8}]}\n\
         Rules:\n\
         - \"surface\" (for an entity) and \"quote\" (for a fact) are copied verbatim from the \
         text, character for character; \"offset\" is the 0-based character position where \
         they start in the text.\n\
         - \"name\" is the entity's full canonical name; use the same name for the same entity \
         everywhere, including in facts.\n\
         - Report every mention of an entity, each with its own surface and offset.\n\
         - \"confidence\" is a number from 0 to 1.\n",
    );
    p.push_str(&format!("Entity types: {}\n", options.types.join(", ")));
    match options.relations {
        Some(relations) if !relations.is_empty() => {
            p.push_str(&format!(
                "Relations (use only these): {}\n",
                relations.join(", ")
            ));
        }
        _ => p.push_str("Relations: name each one yourself in UPPER_SNAKE_CASE.\n"),
    }
    if !options.entities {
        p.push_str("Leave \"entities\" empty; report facts only.\n");
    }
    if !options.facts {
        p.push_str("Leave \"facts\" empty; report entities only.\n");
    }
    if !options.language.eq_ignore_ascii_case("auto") {
        p.push_str(&format!(
            "The text is in {}; keep names and quotes in that language.\n",
            options.language
        ));
    }
    if let Some(hint) = options.schema_hint.filter(|h| !h.trim().is_empty()) {
        p.push_str(&format!("Domain notes: {hint}\n"));
    }
    p
}

/// The user message carrying the text to read.
#[must_use]
pub fn user_message(text: &str) -> String {
    format!("Text:\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    fn options<'a>(types: &'a [String], relations: Option<&'a [String]>) -> PromptOptions<'a> {
        PromptOptions {
            types,
            relations,
            schema_hint: None,
            language: "auto",
            entities: true,
            facts: true,
        }
    }

    #[test]
    fn the_prompt_lists_the_types() {
        let types = strings(&["PERSON", "SPACESHIP"]);
        let p = system_prompt(&options(&types, None));
        assert!(p.contains("Entity types: PERSON, SPACESHIP"), "{p}");
    }

    #[test]
    fn the_prompt_lists_the_allowed_relations_or_lets_the_model_choose() {
        let types = strings(&["PERSON"]);
        let relations = strings(&["WORKS_AT", "BORN_IN"]);
        let p = system_prompt(&options(&types, Some(&relations)));
        assert!(
            p.contains("Relations (use only these): WORKS_AT, BORN_IN"),
            "{p}"
        );
        let p = system_prompt(&options(&types, None));
        assert!(p.contains("UPPER_SNAKE_CASE"), "{p}");
    }

    #[test]
    fn the_prompt_carries_the_schema_hint_and_language() {
        let types = strings(&["PERSON"]);
        let mut o = options(&types, None);
        o.schema_hint = Some("Characters are crew of the Nostromo.");
        o.language = "ru";
        let p = system_prompt(&o);
        assert!(p.contains("Characters are crew of the Nostromo."), "{p}");
        assert!(p.contains("The text is in ru"), "{p}");
        let p = system_prompt(&options(&types, None));
        assert!(!p.contains("The text is in"), "auto adds no hint: {p}");
    }

    #[test]
    fn the_prompt_fixes_the_json_contract() {
        let types = strings(&["PERSON"]);
        let p = system_prompt(&options(&types, None));
        for needle in [
            "\"entities\"",
            "\"facts\"",
            "\"surface\"",
            "\"quote\"",
            "\"offset\"",
            "verbatim",
            "ONLY",
        ] {
            assert!(p.contains(needle), "missing {needle}: {p}");
        }
    }

    #[test]
    fn the_prompt_says_which_lists_to_fill() {
        let types = strings(&["PERSON"]);
        let mut o = options(&types, None);
        o.facts = false;
        let p = system_prompt(&o);
        assert!(p.contains("Leave \"facts\" empty"), "{p}");
        o.facts = true;
        o.entities = false;
        let p = system_prompt(&o);
        assert!(p.contains("Leave \"entities\" empty"), "{p}");
    }

    #[test]
    fn the_user_message_carries_the_text_verbatim() {
        let m = user_message("Дана 🚀 joined Acme.");
        assert!(m.contains("Дана 🚀 joined Acme."));
    }
}
