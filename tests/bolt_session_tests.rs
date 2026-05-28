//! Integration tests for the Bolt session layer — Phase 11 task `00071`.
//!
//! The session sits on top of the codec (`00070`) and drives the
//! HELLO → READY → STREAMING → READY state machine that official Neo4j
//! drivers expect. These tests pin the encoded byte shapes, the state
//! transitions, and the wiring through `cypher::parser` +
//! `cypher::executor` (Phase 10, `00063` onward).

#![cfg(all(not(target_arch = "wasm32"), feature = "redb-backend"))]

use std::collections::BTreeMap;

use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::{
    decode_client, encode_server, run_session_sync, ClientMessage, ServerMessage, Session, State,
    BEGIN, COMMIT, DISCARD, FAILURE, GOODBYE, HELLO, IGNORED, PULL, RECORD, RESET, ROLLBACK, RUN,
    SUCCESS,
};
use drevo::db::Drevo;

fn open() -> Drevo {
    Drevo::open_in_memory().expect("open_in_memory")
}

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn extract_dict(value: Value) -> BTreeMap<String, Value> {
    match value {
        Value::Dictionary(m) => m,
        other => panic!("expected dictionary, got {other:?}"),
    }
}

fn extract_struct(value: Value) -> (u8, Vec<Value>) {
    match value {
        Value::Structure { tag, fields } => (tag, fields),
        other => panic!("expected structure, got {other:?}"),
    }
}

// --- Tag byte constants -----------------------------------------------------

#[test]
fn message_tag_constants_match_bolt_v4_spec() {
    assert_eq!(HELLO, 0x01);
    assert_eq!(GOODBYE, 0x02);
    assert_eq!(RESET, 0x0F);
    assert_eq!(RUN, 0x10);
    assert_eq!(BEGIN, 0x11);
    assert_eq!(COMMIT, 0x12);
    assert_eq!(ROLLBACK, 0x13);
    assert_eq!(DISCARD, 0x2F);
    assert_eq!(PULL, 0x3F);
    assert_eq!(SUCCESS, 0x70);
    assert_eq!(RECORD, 0x71);
    assert_eq!(IGNORED, 0x7E);
    assert_eq!(FAILURE, 0x7F);
}

// --- Client message decoding ------------------------------------------------

#[test]
fn decode_hello_extracts_extra_dict() {
    let extra = dict([
        ("user_agent", Value::String("driver/4.4".to_string())),
        ("scheme", Value::String("none".to_string())),
    ]);
    let struct_val = Value::Structure {
        tag: HELLO,
        fields: vec![Value::Dictionary(extra.clone())],
    };
    let msg = decode_client(&struct_val).expect("decode hello");
    match msg {
        ClientMessage::Hello { extra: e } => assert_eq!(e, extra),
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn decode_goodbye_has_no_fields() {
    let struct_val = Value::Structure {
        tag: GOODBYE,
        fields: vec![],
    };
    let msg = decode_client(&struct_val).expect("decode goodbye");
    assert_eq!(msg, ClientMessage::Goodbye);
}

#[test]
fn decode_reset_has_no_fields() {
    let struct_val = Value::Structure {
        tag: RESET,
        fields: vec![],
    };
    let msg = decode_client(&struct_val).expect("decode reset");
    assert_eq!(msg, ClientMessage::Reset);
}

#[test]
fn decode_run_extracts_query_params_extra() {
    let params = dict([("n", Value::Integer(42))]);
    let extra = dict([("mode", Value::String("r".to_string()))]);
    let struct_val = Value::Structure {
        tag: RUN,
        fields: vec![
            Value::String("RETURN $n".to_string()),
            Value::Dictionary(params.clone()),
            Value::Dictionary(extra.clone()),
        ],
    };
    let msg = decode_client(&struct_val).expect("decode run");
    match msg {
        ClientMessage::Run {
            query,
            parameters,
            extra: e,
        } => {
            assert_eq!(query, "RETURN $n");
            assert_eq!(parameters, params);
            assert_eq!(e, extra);
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn decode_pull_extracts_n_and_qid() {
    let extra = dict([("n", Value::Integer(-1))]);
    let struct_val = Value::Structure {
        tag: PULL,
        fields: vec![Value::Dictionary(extra.clone())],
    };
    let msg = decode_client(&struct_val).expect("decode pull");
    match msg {
        ClientMessage::Pull { extra: e } => assert_eq!(e, extra),
        other => panic!("expected Pull, got {other:?}"),
    }
}

#[test]
fn decode_discard_extracts_extra() {
    let extra = dict([("n", Value::Integer(100))]);
    let struct_val = Value::Structure {
        tag: DISCARD,
        fields: vec![Value::Dictionary(extra.clone())],
    };
    let msg = decode_client(&struct_val).expect("decode discard");
    match msg {
        ClientMessage::Discard { extra: e } => assert_eq!(e, extra),
        other => panic!("expected Discard, got {other:?}"),
    }
}

#[test]
fn decode_rejects_non_structure_value() {
    let err = decode_client(&Value::Null).unwrap_err();
    // Must reject — non-structure values are never valid Bolt messages.
    let _ = err;
}

#[test]
fn decode_rejects_unknown_tag() {
    let struct_val = Value::Structure {
        tag: 0xAB,
        fields: vec![],
    };
    let err = decode_client(&struct_val).unwrap_err();
    let _ = err;
}

// --- Server message encoding ------------------------------------------------

#[test]
fn encode_success_emits_structure_tag_0x70() {
    let metadata = dict([("server", Value::String("drevo/0.1.0".to_string()))]);
    let value = encode_server(&ServerMessage::Success { metadata });
    let (tag, fields) = extract_struct(value);
    assert_eq!(tag, SUCCESS);
    assert_eq!(fields.len(), 1);
    let md = extract_dict(fields.into_iter().next().unwrap());
    assert_eq!(
        md.get("server"),
        Some(&Value::String("drevo/0.1.0".to_string()))
    );
}

#[test]
fn encode_failure_emits_structure_tag_0x7f() {
    let metadata = dict([
        (
            "code",
            Value::String("Neo.ClientError.Statement.SyntaxError".to_string()),
        ),
        ("message", Value::String("bad query".to_string())),
    ]);
    let value = encode_server(&ServerMessage::Failure { metadata });
    let (tag, fields) = extract_struct(value);
    assert_eq!(tag, FAILURE);
    assert_eq!(fields.len(), 1);
}

#[test]
fn encode_record_emits_structure_tag_0x71_with_list_field() {
    let value = encode_server(&ServerMessage::Record {
        fields: vec![Value::Integer(1), Value::String("a".to_string())],
    });
    let (tag, fields) = extract_struct(value);
    assert_eq!(tag, RECORD);
    assert_eq!(fields.len(), 1);
    match &fields[0] {
        Value::List(items) => assert_eq!(items.len(), 2),
        other => panic!("expected list, got {other:?}"),
    }
}

#[test]
fn encode_ignored_emits_empty_structure_tag_0x7e() {
    let value = encode_server(&ServerMessage::Ignored);
    let (tag, fields) = extract_struct(value);
    assert_eq!(tag, IGNORED);
    assert!(fields.is_empty());
}

#[test]
fn server_message_roundtrips_through_packstream_bytes() {
    let msg = ServerMessage::Success {
        metadata: dict([("connection_id", Value::String("bolt-1".to_string()))]),
    };
    let value = encode_server(&msg);
    let mut buf = Vec::new();
    encode(&value, &mut buf).unwrap();
    let (decoded, rest) = decode(&buf).unwrap();
    assert!(rest.is_empty());
    let (tag, _) = extract_struct(decoded);
    assert_eq!(tag, SUCCESS);
}

// --- State machine: HELLO -----------------------------------------------------

#[test]
fn new_session_starts_in_connected_state() {
    let drevo = open();
    let s = Session::new(&drevo);
    assert_eq!(s.state(), State::Connected);
}

#[test]
fn hello_in_connected_transitions_to_ready_with_success() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    let replies = s.handle(ClientMessage::Hello {
        extra: dict([("user_agent", Value::String("test/1".to_string()))]),
    });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Success { metadata } => {
            assert!(metadata.contains_key("server"));
            assert!(metadata.contains_key("connection_id"));
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn hello_when_already_ready_yields_failure() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Hello { extra: dict([]) });
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
    assert_eq!(s.state(), State::Failed);
}

#[test]
fn run_before_hello_yields_failure() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    let replies = s.handle(ClientMessage::Run {
        query: "RETURN 1".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
}

// --- State machine: RUN / PULL / DISCARD --------------------------------------

#[test]
fn run_return_literal_transitions_to_streaming_with_field_metadata() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Run {
        query: "RETURN 1 AS one, 2 AS two".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Success { metadata } => {
            let fields = metadata.get("fields").expect("fields metadata");
            match fields {
                Value::List(items) => {
                    let names: Vec<String> = items
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => panic!("non-string field name: {other:?}"),
                        })
                        .collect();
                    assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
                }
                other => panic!("expected list, got {other:?}"),
            }
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Streaming);
}

#[test]
fn pull_all_after_run_emits_record_then_success_with_has_more_false() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "RETURN 1 AS n".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert_eq!(replies.len(), 2, "expected 1 RECORD + 1 SUCCESS");
    match &replies[0] {
        ServerMessage::Record { fields } => {
            assert_eq!(fields, &vec![Value::Integer(1)]);
        }
        other => panic!("expected Record, got {other:?}"),
    }
    match &replies[1] {
        ServerMessage::Success { metadata } => match metadata.get("has_more") {
            Some(Value::Boolean(false)) | None => {}
            other => panic!("expected has_more=false or absent, got {other:?}"),
        },
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn pull_with_finite_n_leaves_has_more_true_when_more_remain() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });

    // Seed four nodes so the executor will return four rows on MATCH.
    s.handle(ClientMessage::Run {
        query: "CREATE (:Row {x: 1}), (:Row {x: 2}), (:Row {x: 3}), (:Row {x: 4})".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert_eq!(s.state(), State::Ready);

    s.handle(ClientMessage::Run {
        query: "MATCH (r:Row) RETURN r.x AS x ORDER BY x".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(2))]),
    });
    // Expect 2 RECORDs + 1 SUCCESS{has_more:true}; session must still be
    // in Streaming so the client can PULL/DISCARD the rest.
    assert_eq!(replies.len(), 3);
    let records: Vec<_> = replies
        .iter()
        .filter(|r| matches!(r, ServerMessage::Record { .. }))
        .collect();
    assert_eq!(records.len(), 2);
    match replies.last().expect("trailing success") {
        ServerMessage::Success { metadata } => {
            assert_eq!(metadata.get("has_more"), Some(&Value::Boolean(true)));
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Streaming);

    // Pull the remaining two — the session should drop back to Ready.
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    let trailing = replies.last().expect("trailing success");
    match trailing {
        ServerMessage::Success { metadata } => {
            assert_eq!(metadata.get("has_more"), Some(&Value::Boolean(false)));
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn discard_all_after_run_skips_records_and_returns_to_ready() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "RETURN 1 AS n".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Discard {
        extra: dict([("n", Value::Integer(-1))]),
    });
    // DISCARD never emits RECORD — just a terminal SUCCESS.
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Success { .. }));
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn pull_without_active_stream_yields_failure() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
}

// --- State machine: FAILED + IGNORED ----------------------------------------

#[test]
fn run_with_syntax_error_yields_failure_and_transitions_to_failed() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Run {
        query: "THIS IS NOT CYPHER ¶".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Failure { metadata } => {
            let code = metadata.get("code").expect("code");
            match code {
                Value::String(s) => assert!(s.contains("Neo."), "code = {s}"),
                other => panic!("expected string code, got {other:?}"),
            }
            assert!(metadata.contains_key("message"));
        }
        other => panic!("expected Failure, got {other:?}"),
    }
    assert_eq!(s.state(), State::Failed);
}

#[test]
fn messages_in_failed_state_are_ignored_until_reset() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "*** garbage ***".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(s.state(), State::Failed);
    let replies = s.handle(ClientMessage::Run {
        query: "RETURN 1".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies, vec![ServerMessage::Ignored]);
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert_eq!(replies, vec![ServerMessage::Ignored]);
}

#[test]
fn reset_from_failed_clears_state_and_returns_success() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "*** garbage ***".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Reset);
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Success { .. }));
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn reset_from_streaming_drops_pending_records() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "RETURN 1 AS n".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(s.state(), State::Streaming);
    let replies = s.handle(ClientMessage::Reset);
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Success { .. }));
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn reset_from_ready_is_idempotent() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Reset);
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Success { .. }));
    assert_eq!(s.state(), State::Ready);
}

// --- GOODBYE ---------------------------------------------------------------

#[test]
fn goodbye_in_ready_returns_no_reply_and_marks_defunct() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Goodbye);
    assert!(replies.is_empty());
    assert_eq!(s.state(), State::Defunct);
}

#[test]
fn goodbye_from_connected_state_also_defuncts() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    let replies = s.handle(ClientMessage::Goodbye);
    assert!(replies.is_empty());
    assert_eq!(s.state(), State::Defunct);
}

// --- BEGIN/COMMIT/ROLLBACK reserved for 00072 ------------------------------

#[test]
fn begin_yields_failure_explicitly_pointing_to_00072() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Begin { extra: dict([]) });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Failure { metadata } => {
            let msg = metadata.get("message").expect("message");
            match msg {
                Value::String(s) => assert!(s.contains("00072"), "message = {s}"),
                other => panic!("expected string message, got {other:?}"),
            }
        }
        other => panic!("expected Failure, got {other:?}"),
    }
}

#[test]
fn commit_yields_failure_explicitly_pointing_to_00072() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Commit);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
}

#[test]
fn rollback_yields_failure_explicitly_pointing_to_00072() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Rollback);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
}

// --- Cypher integration -----------------------------------------------------

#[test]
fn run_create_then_run_match_round_trips_via_session() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });

    // CREATE one node — no RETURN, so PULL yields just a terminal SUCCESS.
    s.handle(ClientMessage::Run {
        query: "CREATE (:Person {name: 'alice'})".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert!(replies
        .iter()
        .any(|r| matches!(r, ServerMessage::Success { .. })));
    assert_eq!(s.state(), State::Ready);

    // MATCH should now find one node.
    s.handle(ClientMessage::Run {
        query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    let records: Vec<_> = replies
        .iter()
        .filter_map(|r| {
            if let ServerMessage::Record { fields } = r {
                Some(fields.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], vec![Value::String("alice".to_string())]);
}

#[test]
fn run_uses_parameter_via_packstream_to_cypher_conversion() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "RETURN $n AS n".to_string(),
        parameters: dict([("n", Value::Integer(42))]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    let records: Vec<_> = replies
        .iter()
        .filter_map(|r| {
            if let ServerMessage::Record { fields } = r {
                Some(fields.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], vec![Value::Integer(42)]);
}

#[test]
fn run_returning_a_node_emits_bolt_node_structure_tag_0x4e() {
    let drevo = open();
    let mut s = Session::new(&drevo);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s.handle(ClientMessage::Run {
        query: "CREATE (n:Person {name: 'bob'}) RETURN n".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    let replies = s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    let record_fields: Vec<_> = replies
        .iter()
        .find_map(|r| {
            if let ServerMessage::Record { fields } = r {
                Some(fields.clone())
            } else {
                None
            }
        })
        .expect("a record");
    assert_eq!(record_fields.len(), 1);
    let (tag, fields) = extract_struct(record_fields.into_iter().next().unwrap());
    // Bolt v4 spec: Node structure tag = 0x4E with 3 fields (id, labels, properties).
    assert_eq!(tag, 0x4E);
    assert_eq!(fields.len(), 3);
    match &fields[0] {
        Value::Integer(_) => {}
        other => panic!("expected Integer id, got {other:?}"),
    }
    match &fields[1] {
        Value::List(labels) => {
            assert_eq!(labels.len(), 1);
            assert_eq!(labels[0], Value::String("Person".to_string()));
        }
        other => panic!("expected list of labels, got {other:?}"),
    }
    match &fields[2] {
        Value::Dictionary(props) => {
            assert_eq!(props.get("name"), Some(&Value::String("bob".to_string())));
        }
        other => panic!("expected dict of properties, got {other:?}"),
    }
}

// --- run_session_sync end-to-end driver ------------------------------------

#[test]
fn run_session_sync_drives_full_hello_run_pull_goodbye_flow() {
    use drevo::bolt::chunked::{read_message, write_message};
    use std::io::Cursor;

    let drevo = open();

    // Build the client request stream: HELLO, RUN, PULL, GOODBYE — each
    // PackStream-encoded then chunked.
    let mut client_bytes: Vec<u8> = Vec::new();
    for msg in [
        ClientMessage::Hello {
            extra: dict([("user_agent", Value::String("driver/1".to_string()))]),
        },
        ClientMessage::Run {
            query: "RETURN 1 AS n".to_string(),
            parameters: dict([]),
            extra: dict([]),
        },
        ClientMessage::Pull {
            extra: dict([("n", Value::Integer(-1))]),
        },
        ClientMessage::Goodbye,
    ] {
        let value = client_message_to_value(&msg);
        let mut payload = Vec::new();
        encode(&value, &mut payload).unwrap();
        write_message(&payload, &mut client_bytes).unwrap();
    }

    let mut reader = Cursor::new(client_bytes);
    let mut writer: Vec<u8> = Vec::new();
    run_session_sync(&mut reader, &mut writer, &drevo).expect("session loop");

    // Drain the server reply stream: one chunked message per response.
    let mut server_msgs = Vec::new();
    let mut cur = Cursor::new(writer);
    while let Ok(payload) = read_message(&mut cur) {
        let (val, rest) = decode(&payload).unwrap();
        assert!(rest.is_empty());
        server_msgs.push(val);
    }

    // Expected sequence (HELLO→Success, RUN→Success{fields},
    // PULL→Record(1) + Success{has_more=false}).
    assert!(server_msgs.len() >= 4);
    let tags: Vec<u8> = server_msgs
        .iter()
        .map(|v| extract_struct(v.clone()).0)
        .collect();
    assert_eq!(tags[0], SUCCESS); // HELLO ack
    assert_eq!(tags[1], SUCCESS); // RUN ack
    assert_eq!(tags[2], RECORD); // PULL record
    assert_eq!(tags[3], SUCCESS); // PULL ack
}

fn client_message_to_value(msg: &ClientMessage) -> Value {
    match msg {
        ClientMessage::Hello { extra } => Value::Structure {
            tag: HELLO,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Goodbye => Value::Structure {
            tag: GOODBYE,
            fields: vec![],
        },
        ClientMessage::Reset => Value::Structure {
            tag: RESET,
            fields: vec![],
        },
        ClientMessage::Run {
            query,
            parameters,
            extra,
        } => Value::Structure {
            tag: RUN,
            fields: vec![
                Value::String(query.clone()),
                Value::Dictionary(parameters.clone()),
                Value::Dictionary(extra.clone()),
            ],
        },
        ClientMessage::Begin { extra } => Value::Structure {
            tag: BEGIN,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Commit => Value::Structure {
            tag: COMMIT,
            fields: vec![],
        },
        ClientMessage::Rollback => Value::Structure {
            tag: ROLLBACK,
            fields: vec![],
        },
        ClientMessage::Discard { extra } => Value::Structure {
            tag: DISCARD,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Pull { extra } => Value::Structure {
            tag: PULL,
            fields: vec![Value::Dictionary(extra.clone())],
        },
    }
}
