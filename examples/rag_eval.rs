//! Compare retrieval strategies on your own corpus: runs
//! `drevo.rag.evaluateSummary` over a gold set and prints the result as a
//! markdown table.
//!
//! ```sh
//! cargo run --example rag_eval -- --data path.drevo --questions q.jsonl \
//!     --strategies fts,hybrid+expand,rag --budget 2000
//! ```
//!
//! `q.jsonl` holds one question per line, in the shape `drevo.rag.evaluate`
//! takes:
//!
//! ```json
//! {"query": "login fails on iOS", "expected": [{"name": "EXC_BAD_ACCESS crash", "label": "CrashLog"}, 42]}
//! {"query": "why the rollback?", "traceId": "8c1f…"}
//! ```
//!
//! Options: `--strategies` (comma-separated; default: every strategy the
//! store can run), `--budget` (the `tokenBudget`, default 2000), `--label`
//! and `--k` (passed through). The store is opened read-only, so it can be
//! one a running server has open only when that server is stopped. There is
//! no embedder here, so `vector` is not available.

use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

const SUMMARY: &str = "CALL drevo.rag.evaluateSummary($questions, $config) \
     YIELD strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
     meanTokens \
     RETURN strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, \
     meanTokens";

const USAGE: &str = "usage: rag_eval --data <store> --questions <q.jsonl> \
     [--strategies a,b] [--budget N] [--label L] [--k N]";

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Integer)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or(Value::Null),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

fn cell(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:.3}"),
        other => format!("{other:?}"),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rag_eval: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut options: HashMap<String, String> = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument `{flag}`"))?;
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        options.insert(key.to_string(), value);
    }
    let data = options.remove("data").ok_or("--data is required")?;
    let questions_path = options
        .remove("questions")
        .ok_or("--questions is required")?;

    let mut config: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(list) = options.remove("strategies") {
        let names = list
            .split(',')
            .map(|s| Value::String(s.trim().to_string()))
            .collect();
        config.insert("strategies".to_string(), Value::List(names));
    }
    let budget: i64 = options.remove("budget").map_or(Ok(2000), |b| b.parse())?;
    config.insert("tokenBudget".to_string(), Value::Integer(budget));
    if let Some(label) = options.remove("label") {
        config.insert("label".to_string(), Value::String(label));
    }
    if let Some(k) = options.remove("k") {
        config.insert("k".to_string(), Value::Integer(k.parse()?));
    }
    if let Some(unknown) = options.keys().next() {
        return Err(format!("unknown option --{unknown}").into());
    }

    let mut questions = Vec::new();
    for (i, line) in std::fs::read_to_string(&questions_path)?
        .lines()
        .enumerate()
    {
        if line.trim().is_empty() {
            continue;
        }
        let json: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("{questions_path}:{}: {e}", i + 1))?;
        questions.push(json_to_value(&json));
    }

    let db = NativeService::open_read_only(&data)?;
    let params = HashMap::from([
        ("questions".to_string(), Value::List(questions)),
        ("config".to_string(), Value::Map(config)),
    ]);
    let result = db.execute(&parse(SUMMARY)?, params)?;

    println!(
        "| strategy | questions | hit rate | recall | precision | MRR | p50 ms | p95 ms | tokens |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(cell).collect();
        println!("| {} |", cells.join(" | "));
    }
    Ok(())
}
