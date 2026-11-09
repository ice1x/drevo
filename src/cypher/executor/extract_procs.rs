//! The `drevo.extract.*` procedures: entity and fact extraction with the
//! process-wide chat endpoint ([`crate::extract`]), and `ingest`, which
//! writes what it finds through the long-term memory procedures.

use super::*;
use crate::cypher::provenance::{default_chunk_id, ChunkRef, ChunkSpec, Provenance, SourceRef};
use crate::extract::{self, ChunkExtraction, ExtractOptions, Want};

/// `CALL drevo.extract.entities(text[, config])`.
pub(super) const EXTRACT_ENTITIES: &str = "drevo.extract.entities";
/// `CALL drevo.extract.facts(text[, config])`.
pub(super) const EXTRACT_FACTS: &str = "drevo.extract.facts";
/// `CALL drevo.extract.ingest(text, config)`.
pub(super) const EXTRACT_INGEST: &str = "drevo.extract.ingest";
/// `CALL drevo.extract.chunks(text[, config])`.
pub(super) const EXTRACT_CHUNKS: &str = "drevo.extract.chunks";
/// `CALL drevo.extract.status()`.
pub(super) const EXTRACT_STATUS: &str = "drevo.extract.status";

/// Whether `name` is one of the `drevo.extract.*` procedures.
pub(super) fn is_extract_procedure(name: &str) -> bool {
    matches!(
        name,
        EXTRACT_ENTITIES | EXTRACT_FACTS | EXTRACT_INGEST | EXTRACT_CHUNKS | EXTRACT_STATUS
    )
}

/// The `InvalidProcedureCall` of procedure `name`.
fn refusal(name: &str, message: String, span: Span) -> ExecError {
    ExecError::InvalidProcedureCall {
        name: name.to_string(),
        message,
        span,
    }
}

/// The error of a model-backed procedure run without a chat endpoint.
fn not_configured(name: &str) -> ExecError {
    ExecError::EngineCapability {
        feature: format!(
            "{name} (no chat upstream is configured: set DREVO_EXTRACT_UPSTREAM or \
             DREVO_TEXT2CYPHER_UPSTREAM; see CALL drevo.extract.status())"
        ),
    }
}

/// The `rememberEntity` resolution an ingest's `resolve` asks for, at the
/// default threshold.
fn resolve_options(resolve: extract::Resolve) -> ResolveOptions {
    ResolveOptions {
        resolve: match resolve {
            extract::Resolve::Exact => Resolve::Exact,
            extract::Resolve::Fuzzy => Resolve::Fuzzy,
        },
        ..ResolveOptions::default()
    }
}

/// A character count or offset as a Cypher integer.
fn int(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

/// The `:Entity` label of an extracted type: the POLE+O one, else the type
/// in Pascal case (`SPACE_SHIP` → `SpaceShip`).
fn entity_label(entity_type: &str) -> String {
    if let Some((_, label)) = MEMORY_ENTITY_TYPES.iter().find(|(t, _)| *t == entity_type) {
        return (*label).to_string();
    }
    entity_type
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect()
}

/// The error of an ingest whose extraction failed.
pub(crate) fn ingest_failed(e: &extract::ExtractError, span: Span) -> ExecError {
    match e {
        extract::ExtractError::NotConfigured => not_configured(EXTRACT_INGEST),
        e => refusal(EXTRACT_INGEST, e.to_string(), span),
    }
}

/// What a statement that opens with `CALL drevo.extract.ingest(text,
/// config)` will ingest, known before it runs.
pub(crate) struct UpfrontIngest {
    /// The text argument.
    pub(crate) text: String,
    /// The config argument, read.
    pub(crate) options: ExtractOptions,
    /// The span of the call.
    pub(crate) span: Span,
}

/// Whether `query` opens with a `drevo.extract.ingest` call: one part (no
/// `UNION`) whose first clause is the call, so it runs once.
pub(crate) fn opens_with_ingest(query: &Query) -> bool {
    upfront_call(query).is_some()
}

/// The opening `drevo.extract.ingest` call of `query` ([`opens_with_ingest`]).
fn upfront_call(query: &Query) -> Option<&CallClause> {
    let [part] = query.parts.as_slice() else {
        return None;
    };
    match part.query.clauses.first() {
        Some(Clause::Call(c)) if c.name.join(".") == EXTRACT_INGEST && !c.args.is_empty() => {
            Some(c)
        }
        _ => None,
    }
}

/// The text and options of the opening `drevo.extract.ingest` call of
/// `query`, read on `engine` with `params` before the statement runs; `None`
/// when the statement does not open with the call, or when the call would
/// fail before asking the model (an argument that is not known yet or not
/// valid, no source, an unknown source id, no model installed): the
/// statement then runs as it is and fails the way it always has.
pub(crate) fn upfront_ingest(
    query: &Query,
    engine: &dyn GraphEngine,
    ctx: &NativeQueryContext<'_>,
    params: HashMap<String, Value>,
) -> Option<UpfrontIngest> {
    let c = upfront_call(query)?;
    extract::installed()?;
    // A database the user may not use: the statement says so itself.
    if ctx
        .restriction
        .is_some_and(|r| !r.context.can_access(r.database))
    {
        return None;
    }
    let exec = Executor::new(engine, ctx, params);
    let (text, options) = exec
        .extract_args(EXTRACT_INGEST, c, &Bindings::new(), true)
        .ok()?;
    match options.source.as_ref()? {
        SourceRef::Id(id) => {
            exec.memory_source(EXTRACT_INGEST, id, c.span).ok()?;
        }
        SourceRef::Spec(_) => {}
    }
    Some(UpfrontIngest {
        text,
        options,
        span: c.span,
    })
}

impl Executor<'_> {
    /// Run one of the `drevo.extract.*` procedures, once per input row.
    pub(super) fn run_extract(
        &mut self,
        name: &str,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        match name {
            EXTRACT_INGEST => {
                self.run_per_row_mut(c, columns, &mut |exec, row| exec.extract_ingest(c, row))
            }
            EXTRACT_STATUS => self.run_per_row(c, columns, &mut |_, _| Ok(vec![status_row()])),
            EXTRACT_CHUNKS => self.run_per_row(c, columns, &mut |exec, row| {
                let (text, options) = exec.extract_args(EXTRACT_CHUNKS, c, row, false)?;
                Ok(extract::chunk::split(&text, options.chunk_chars)
                    .into_iter()
                    .map(|chunk| {
                        vec![
                            int(chunk.order),
                            int(chunk.offset),
                            int(chunk.length),
                            Value::String(chunk.text),
                        ]
                    })
                    .collect())
            }),
            _ => {
                let want = if name == EXTRACT_FACTS {
                    Want::Facts
                } else {
                    Want::Entities
                };
                self.run_per_row(c, columns, &mut |exec, row| {
                    exec.extract_read(name, c, row, want)
                })
            }
        }
    }

    /// The text and config arguments of a `drevo.extract.*` call: a `null`
    /// text is empty.
    fn extract_args(
        &self,
        name: &str,
        c: &CallClause,
        row: &Bindings,
        ingest: bool,
    ) -> ExecResultT<(String, ExtractOptions)> {
        let text = match self.eval(&c.args[0], row)? {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => {
                return Err(refusal(
                    name,
                    format!("the text must be a string, got {}", other.type_name()),
                    c.span,
                ))
            }
        };
        let config = match c.args.get(1) {
            Some(arg) => self.eval(arg, row)?,
            None => Value::Null,
        };
        let options =
            ExtractOptions::from_value(&config, ingest).map_err(|m| refusal(name, m, c.span))?;
        Ok((text, options))
    }

    /// Run the extraction of `text` with the installed chat endpoint,
    /// recording its outcome for `drevo.extract.status()`.
    fn extract_run<T>(
        name: &str,
        span: Span,
        run: impl FnOnce(&dyn crate::llm::ChatCompleter) -> Result<T, extract::ExtractError>,
        skipped: impl Fn(&T) -> usize,
    ) -> ExecResultT<T> {
        let completer = extract::installed().ok_or_else(|| not_configured(name))?;
        match run(completer.as_ref()) {
            Ok(found) => {
                extract::record(Ok(skipped(&found)));
                Ok(found)
            }
            Err(e) => {
                extract::record(Err(&e));
                Err(refusal(name, e.to_string(), span))
            }
        }
    }

    /// `drevo.extract.entities` / `drevo.extract.facts`: the located items as
    /// rows, offsets into the whole text.
    fn extract_read(
        &self,
        name: &str,
        c: &CallClause,
        row: &Bindings,
        want: Want,
    ) -> ExecResultT<Rows> {
        let (text, options) = self.extract_args(name, c, row, false)?;
        let found = Self::extract_run(
            name,
            c.span,
            |completer| extract::extract_text(completer, &text, &options, want),
            |found| found.skipped,
        )?;
        let rows = match want {
            Want::Facts => found
                .facts
                .into_iter()
                .map(|f| {
                    vec![
                        Value::String(f.subject),
                        Value::String(f.subject_type),
                        Value::String(f.relation),
                        Value::String(f.object),
                        Value::String(f.object_type),
                        Value::String(f.quote),
                        int(f.offset),
                        int(f.length),
                        Value::Float(f.confidence),
                    ]
                })
                .collect(),
            _ => found
                .entities
                .into_iter()
                .map(|e| {
                    vec![
                        Value::String(e.name),
                        Value::String(e.entity_type),
                        e.description.map_or(Value::Null, Value::String),
                        int(e.offset),
                        int(e.length),
                        Value::String(e.surface),
                        Value::Float(e.confidence),
                    ]
                })
                .collect(),
        };
        Ok(rows)
    }

    /// `drevo.extract.ingest`: extract every passage first, then (unless
    /// `dryRun`) write the source, its chunks, the mentions and the facts.
    /// One `[source, chunks, entities, facts, skipped]` row.
    fn extract_ingest(&mut self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        const PROC: &str = EXTRACT_INGEST;
        let span = c.span;
        let (text, options) = self.extract_args(PROC, c, row, true)?;
        // An ingest job's item, or a statement opening with this call, was
        // extracted before its write began.
        if extract::prepared_for(&text, &options).is_none() {
            extract::installed().ok_or_else(|| not_configured(PROC))?;
        }
        let source = options
            .source
            .clone()
            .ok_or_else(|| refusal(PROC, "a source is required".into(), span))?;
        let source_id = match &source {
            SourceRef::Id(id) => {
                self.memory_source(PROC, id, span)?;
                id.clone()
            }
            SourceRef::Spec(spec) => spec.id.clone(),
        };
        let found =
            extract::prepare_ingest(&text, &options).map_err(|e| ingest_failed(&e, span))?;
        let entities = found.iter().map(|f| f.accepted.entities.len()).sum();
        let facts = found.iter().map(|f| f.accepted.facts.len()).sum();
        let skipped = found.iter().map(|f| f.accepted.skipped).sum();
        let chunks = found.len();
        if !options.dry_run {
            self.extract_write(
                source,
                &source_id,
                options.session.as_deref(),
                resolve_options(options.resolve),
                found,
                span,
            )?;
        }
        Ok(vec![vec![
            Value::String(source_id),
            int(chunks),
            int(entities),
            int(facts),
            int(skipped),
        ]])
    }

    /// Write an ingest: the source, its chunks (`<sourceId>#<order>`), each
    /// entity mention with its `MENTIONED_IN` edge (a known entity found as
    /// `resolve` says), and each fact (asserted non-exclusive) citing its
    /// chunk and quote.
    fn extract_write(
        &mut self,
        source: SourceRef,
        source_id: &str,
        session: Option<&str>,
        resolve: ResolveOptions,
        found: Vec<ChunkExtraction>,
        span: Span,
    ) -> ExecResultT<()> {
        const PROC: &str = EXTRACT_INGEST;
        let source_node = match source {
            SourceRef::Spec(spec) => self.memory_upsert_source(spec, span)?,
            SourceRef::Id(id) => self.memory_source(PROC, &id, span)?,
        };
        let specs = found
            .iter()
            .map(|f| {
                let order = i64::try_from(f.chunk.order).unwrap_or(i64::MAX);
                ChunkSpec {
                    id: Some(default_chunk_id(source_id, order)),
                    order: Some(order),
                    offset: i64::try_from(f.chunk.offset).ok(),
                    length: i64::try_from(f.chunk.length).ok(),
                    text: f.chunk.text.clone(),
                }
            })
            .collect();
        self.memory_add_chunks_to(PROC, &source_node, source_id, specs, span)?;
        let cite = |chunk_id: &str| Provenance {
            source: Some(SourceRef::Id(source_id.to_string())),
            chunk: Some(ChunkRef::Id(chunk_id.to_string())),
            ..Provenance::default()
        };
        for f in found {
            let order = i64::try_from(f.chunk.order).unwrap_or(i64::MAX);
            let chunk_id = default_chunk_id(source_id, order);
            for e in f.accepted.entities {
                let label = entity_label(&e.entity_type);
                let provenance = Provenance {
                    surface: Some(e.surface),
                    offset: i64::try_from(e.offset).ok(),
                    length: i64::try_from(e.length).ok(),
                    ..cite(&chunk_id)
                };
                self.memory_remember(
                    PROC,
                    session.map(str::to_string),
                    e.name,
                    e.entity_type,
                    &label,
                    e.description,
                    Some(provenance),
                    resolve,
                    span,
                )?;
            }
            for fact in f.accepted.facts {
                let mut ends = Vec::with_capacity(2);
                for (name, entity_type) in [
                    (fact.subject.clone(), fact.subject_type.clone()),
                    (fact.object.clone(), fact.object_type.clone()),
                ] {
                    let label = entity_label(&entity_type);
                    ends.push(self.memory_remember(
                        PROC,
                        None,
                        name,
                        entity_type,
                        &label,
                        None,
                        None,
                        resolve,
                        span,
                    )?);
                }
                let provenance = Provenance {
                    quote: Some(fact.quote),
                    offset: i64::try_from(fact.offset).ok(),
                    length: i64::try_from(fact.length).ok(),
                    ..cite(&chunk_id)
                };
                self.memory_assert(
                    PROC,
                    &ends[0],
                    fact.relation,
                    &ends[1],
                    false,
                    Some(provenance),
                    span,
                )?;
            }
        }
        Ok(())
    }
}

/// The `drevo.extract.status()` row.
fn status_row() -> Vec<Value> {
    let st = extract::status();
    vec![
        Value::Bool(st.configured),
        st.model.map_or(Value::Null, Value::String),
        st.upstream.map_or(Value::Null, Value::String),
        st.last_error.map_or(Value::Null, Value::String),
        st.last_skipped.map_or(Value::Null, |n| {
            Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::entity_label;

    #[test]
    fn labels_are_pole_o_or_pascal_case() {
        assert_eq!(entity_label("PERSON"), "Person");
        assert_eq!(entity_label("SPACESHIP"), "Spaceship");
        assert_eq!(entity_label("SPACE_SHIP"), "SpaceShip");
        assert_eq!(entity_label("ГЕРОЙ"), "Герой");
    }
}
