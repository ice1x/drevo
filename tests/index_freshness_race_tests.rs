//! The service's indexes against commits that land while they catch up: once
//! the writers are done, a lookup by property value finds every node they
//! committed. A commit that lands while the indexes sync must not be counted
//! as synced before every index has read it, or the next statements trust an
//! index that misses it until the following write.

use std::collections::HashMap;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

const ROUNDS: usize = 200;
const WRITERS: usize = 3;
const READERS: usize = 3;
const NOTES: usize = 4;

fn found(svc: &NativeService, title: &str) -> i64 {
    let res = svc
        .execute(
            &parse("MATCH (n:Note {title: $title}) RETURN count(n) AS c").expect("parse"),
            HashMap::from([("title".to_string(), Value::String(title.to_string()))]),
        )
        .expect("lookup");
    match res.rows[0][0] {
        Value::Integer(c) => c,
        ref other => panic!("{other:?}"),
    }
}

/// Writers commit notes, each in its own explicit transaction, while readers
/// keep looking notes up by title, so the indexes catch up between commits.
fn commit_while_reading(svc: &NativeService) {
    let create = parse("CREATE (:Note {title: $title})").expect("parse");
    std::thread::scope(|scope| {
        for w in 0..WRITERS {
            let create = &create;
            scope.spawn(move || {
                for i in 0..NOTES {
                    let params =
                        HashMap::from([("title".to_string(), Value::String(format!("w{w}-{i}")))]);
                    // A conflict with a concurrent commit is retried, as a
                    // driver would.
                    loop {
                        let tx = svc.begin_tx();
                        svc.execute_in_tx(tx, create, params.clone())
                            .expect("create");
                        if svc.commit_tx(tx).is_ok() {
                            break;
                        }
                    }
                }
            });
        }
        for r in 0..READERS {
            scope.spawn(move || {
                for i in 0..NOTES * 2 {
                    found(svc, &format!("w{}-{}", r % WRITERS, i % NOTES));
                }
            });
        }
    });
}

#[test]
fn property_lookups_find_every_note_committed_during_index_catch_up() {
    for round in 0..ROUNDS {
        let svc = NativeService::in_memory();
        commit_while_reading(&svc);
        for w in 0..WRITERS {
            for i in 0..NOTES {
                let title = format!("w{w}-{i}");
                assert_eq!(found(&svc, &title), 1, "round {round}: `{title}`");
            }
        }
    }
}
