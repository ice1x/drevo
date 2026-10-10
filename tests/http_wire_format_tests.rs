//! Optional CBOR / MessagePack bodies for the HTTP API (issue #581).
//!
//! JSON stays the default. A format the server compiled in and enabled via
//! `DREVO_HTTP_FORMATS` is chosen per request with `Content-Type` (request
//! body) and `Accept` (response body).

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::model::{NewNode, Properties};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::wire_format::WireFormats;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// A task-manager graph with an embedding on each task. The components are
/// `f32` values widened to `f64`, like the vectors an embedding model returns.
fn app(formats: &str) -> axum::Router {
    let db = Arc::new(NativeService::in_memory());
    for i in 1..=5_u8 {
        let embedding: Vec<f64> = (1..=64_u16)
            .map(|x| f64::from((f32::from(x) * 0.37 + f32::from(i)).sin() * 0.05))
            .collect();
        db.create_node(NewNode {
            kind: "Task".into(),
            title: format!("task-{i}"),
            body: String::new(),
            body_html: String::new(),
            properties: Properties(
                [
                    ("done".to_string(), json!(i % 2 == 0)),
                    ("embedding".to_string(), json!(embedding)),
                ]
                .into_iter()
                .collect(),
            ),
        })
        .unwrap();
    }
    let formats: WireFormats = formats.parse().expect("formats");
    build_native_router(NativeApiState::new(db).with_wire_formats(formats))
}

async fn send(
    app: &axum::Router,
    content_type: &str,
    accept: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", content_type);
    if let Some(a) = accept {
        req = req.header("accept", a);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (ct, vary) = (header("content-type"), header("vary"));
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, ct, vary, bytes)
}

fn query(q: &str) -> Value {
    json!({ "query": q })
}

const Q: &str =
    "MATCH (t:Task) RETURN t.title AS title, t.done AS done, t.embedding AS e ORDER BY title";

#[tokio::test]
async fn json_is_the_default_and_unchanged() {
    let app = app("");
    let (status, ct, vary, body) = send(
        &app,
        "application/json",
        None,
        query(Q).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("application/json"));
    assert_eq!(vary, None, "a JSON-only server does not vary on Accept");
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["rows"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn a_format_that_is_not_enabled_is_refused() {
    let app = app("json");
    let (status, _, _, body) = send(
        &app,
        "application/json",
        Some("application/cbor"),
        query(Q).to_string().into_bytes(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_ACCEPTABLE,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _, _, _) = send(&app, "application/cbor", None, vec![0xa0]).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // A browser-style Accept still gets JSON.
    let (status, ct, _, _) = send(
        &app,
        "application/json",
        Some("text/html,application/xhtml+xml,*/*;q=0.8"),
        query("RETURN 1 AS x").to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("application/json"));
}

#[cfg(all(feature = "format-cbor", feature = "format-msgpack"))]
mod binary {
    use super::*;

    fn cbor(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::into_writer(v, &mut out).unwrap();
        out
    }

    async fn json_answer(app: &axum::Router) -> (Value, usize) {
        let (_, _, _, body) = send(
            app,
            "application/json",
            None,
            query(Q).to_string().into_bytes(),
        )
        .await;
        (serde_json::from_slice(&body).unwrap(), body.len())
    }

    #[tokio::test]
    async fn cbor_and_msgpack_responses_carry_the_same_value() {
        let app = app("json,cbor,msgpack");
        let (want, _) = json_answer(&app).await;

        let (status, ct, vary, body) = send(
            &app,
            "application/json",
            Some("application/cbor"),
            query(Q).to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("application/cbor"));
        assert_eq!(vary.as_deref(), Some("accept"));
        let got: Value = ciborium::from_reader(body.as_slice()).unwrap();
        assert_eq!(got, want);

        let (status, ct, _, body) = send(
            &app,
            "application/json",
            Some("application/msgpack"),
            query(Q).to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.as_deref(), Some("application/msgpack"));
        let got: Value = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn request_bodies_may_be_binary_too() {
        let app = app("cbor,msgpack");
        let (want, _) = json_answer(&app).await;
        let (status, ct, _, body) = send(&app, "application/cbor", None, cbor(&query(Q))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            ct.as_deref(),
            Some("application/json"),
            "Accept decides the response"
        );
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), want);

        let packed = rmp_serde::to_vec_named(&query(Q)).unwrap();
        let (status, _, _, body) = send(
            &app,
            "application/x-msgpack",
            Some("application/msgpack"),
            packed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rmp_serde::from_slice::<Value>(&body).unwrap(), want);
    }

    #[tokio::test]
    async fn a_malformed_binary_body_is_a_bad_request() {
        let app = app("cbor");
        let (status, _, _, _) = send(&app, "application/cbor", None, vec![0xff, 0x00]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn vectors_are_much_smaller_in_cbor() {
        let app = app("cbor");
        let (_, json_len) = json_answer(&app).await;
        let (_, _, _, body) = send(
            &app,
            "application/json",
            Some("application/cbor"),
            query(Q).to_string().into_bytes(),
        )
        .await;
        assert!(
            body.len() * 3 < json_len,
            "cbor {} bytes vs json {json_len}",
            body.len()
        );
    }

    #[tokio::test]
    async fn errors_are_encoded_in_the_accepted_format() {
        let app = app("cbor");
        let (status, ct, _, body) = send(
            &app,
            "application/json",
            Some("application/cbor"),
            query("RETURN nosuchfunction(1)").to_string().into_bytes(),
        )
        .await;
        assert!(status.is_client_error(), "{status}");
        assert_eq!(ct.as_deref(), Some("application/cbor"));
        let v: Value = ciborium::from_reader(body.as_slice()).unwrap();
        assert!(v.get("error").is_some(), "{v}");
    }

    #[tokio::test]
    async fn the_web_ui_is_never_transcoded() {
        let app = app("cbor");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("accept", "application/cbor, text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ct.starts_with("text/html"), "{ct}");
    }
}
