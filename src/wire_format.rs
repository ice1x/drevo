//! Optional binary body formats for the HTTP API (issue #581).
//!
//! JSON is the HTTP API's format and is always available. An operator can
//! additionally enable **CBOR** (`application/cbor`) and **MessagePack**
//! (`application/msgpack`): each is compiled in by a Cargo feature
//! (`format-cbor`, `format-msgpack`) and switched on at run time by listing it
//! in `DREVO_HTTP_FORMATS` (e.g. `json,cbor,msgpack`; the default is `json`).
//!
//! A client then picks the format per request:
//!
//! - **Request body:** `Content-Type: application/cbor` (or msgpack). A body in
//!   a format that is not enabled is answered `415 Unsupported Media Type`.
//! - **Response body:** `Accept: application/cbor` (or msgpack), with the usual
//!   `q` weights. Without `Accept`, or with `*/*`, the answer is JSON. An
//!   `Accept` that names only formats the server does not offer is answered
//!   `406 Not Acceptable`.
//!
//! [`negotiate`](crate::wire_format::negotiate) does this for every route in one place by transcoding: the
//! request body is turned into JSON before the handler runs, and a JSON
//! response is re-encoded on the way out. Handlers are unchanged, and non-JSON
//! responses (the Web UI's HTML/JS/CSS, GraphML) pass through untouched.
//! CBOR writes each float in the shortest form that keeps its value, so an
//! embedding of `f32`s costs 5 bytes per component instead of ~19 in JSON.

#![cfg(feature = "http")]

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, VARY};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// An HTTP body format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireFormat {
    /// `application/json` — the default, always enabled.
    Json,
    /// `application/cbor` (RFC 8949), behind the `format-cbor` feature.
    Cbor,
    /// `application/msgpack`, behind the `format-msgpack` feature.
    MsgPack,
}

impl WireFormat {
    /// Every format, in the server's order of preference when a client's
    /// `Accept` weighs several equally.
    pub const ALL: [WireFormat; 3] = [WireFormat::Json, WireFormat::Cbor, WireFormat::MsgPack];

    /// The canonical media type.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            WireFormat::Json => "application/json",
            WireFormat::Cbor => "application/cbor",
            WireFormat::MsgPack => "application/msgpack",
        }
    }

    /// The name used in `DREVO_HTTP_FORMATS`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WireFormat::Json => "json",
            WireFormat::Cbor => "cbor",
            WireFormat::MsgPack => "msgpack",
        }
    }

    /// Whether this binary was compiled with support for the format.
    #[must_use]
    pub fn compiled_in(self) -> bool {
        match self {
            WireFormat::Json => true,
            WireFormat::Cbor => cfg!(feature = "format-cbor"),
            WireFormat::MsgPack => cfg!(feature = "format-msgpack"),
        }
    }

    /// The format a media type (without parameters, any case) names, if any.
    /// Accepts the common MessagePack aliases.
    #[must_use]
    pub fn from_media_type(media_type: &str) -> Option<WireFormat> {
        let mt = media_type.trim().to_ascii_lowercase();
        match mt.as_str() {
            "application/json" => Some(WireFormat::Json),
            "application/cbor" => Some(WireFormat::Cbor),
            "application/msgpack" | "application/x-msgpack" | "application/vnd.msgpack" => {
                Some(WireFormat::MsgPack)
            }
            _ => None,
        }
    }
}

impl fmt::Display for WireFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a `DREVO_HTTP_FORMATS` value was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatsError {
    /// A name that is not a known format.
    #[error("unknown HTTP format `{0}` (known: json, cbor, msgpack)")]
    Unknown(String),
    /// A known format this binary was built without.
    #[error("HTTP format `{0}` is not compiled in; rebuild with the `format-{0}` feature")]
    NotCompiled(String),
}

/// The set of formats the server offers. JSON is always in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFormats {
    enabled: Vec<WireFormat>,
}

impl Default for WireFormats {
    /// JSON only.
    fn default() -> Self {
        Self {
            enabled: vec![WireFormat::Json],
        }
    }
}

impl FromStr for WireFormats {
    type Err = FormatsError;

    /// Parse a comma-separated list such as `json,cbor`. JSON is added if
    /// missing; blanks are ignored; names are case-insensitive.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut enabled = vec![WireFormat::Json];
        for raw in s.split(',') {
            let name = raw.trim().to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            let format = WireFormat::ALL
                .into_iter()
                .find(|f| f.name() == name)
                .ok_or_else(|| FormatsError::Unknown(name.clone()))?;
            if !format.compiled_in() {
                return Err(FormatsError::NotCompiled(name));
            }
            if !enabled.contains(&format) {
                enabled.push(format);
            }
        }
        Ok(Self { enabled })
    }
}

impl WireFormats {
    /// Whether `format` is offered.
    #[must_use]
    pub fn is_enabled(&self, format: WireFormat) -> bool {
        self.enabled.contains(&format)
    }

    /// The enabled formats, JSON first.
    #[must_use]
    pub fn enabled(&self) -> &[WireFormat] {
        &self.enabled
    }

    /// The response format for an `Accept` header value: the enabled format
    /// with the highest `q` (ties broken by [`WireFormat::ALL`] order).
    /// `Ok(Json)` when the header is absent, or names only types outside this
    /// negotiation (`*/*`, `application/*`, `text/html`, …), so ordinary
    /// clients and browsers keep getting JSON. `Err(())` when the header names
    /// only wire formats and none of them is offered.
    #[allow(clippy::result_unit_err)]
    pub fn response_format(&self, accept: Option<&str>) -> Result<WireFormat, ()> {
        let Some(accept) = accept else {
            return Ok(WireFormat::Json);
        };
        let mut best: Option<(f32, usize, WireFormat)> = None;
        let mut named_a_wire_format = false;
        let mut wildcard_q: Option<f32> = None;
        for range in accept.split(',') {
            let mut parts = range.split(';');
            let media = parts.next().unwrap_or("").trim().to_ascii_lowercase();
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|v| v.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            if media == "*/*" || media == "application/*" {
                wildcard_q = Some(wildcard_q.map_or(q, |w| w.max(q)));
                continue;
            }
            let Some(format) = WireFormat::from_media_type(&media) else {
                continue;
            };
            named_a_wire_format = true;
            if q <= 0.0 || !self.is_enabled(format) {
                continue;
            }
            let rank = WireFormat::ALL
                .iter()
                .position(|f| *f == format)
                .unwrap_or(0);
            let better = best.is_none_or(|(bq, br, _)| q > bq || (q == bq && rank < br));
            if better {
                best = Some((q, rank, format));
            }
        }
        match (best, wildcard_q) {
            // A wildcard weighted above every named format: JSON serves it.
            (Some((q, _, _)), Some(w)) if w > q => Ok(WireFormat::Json),
            (Some((_, _, format)), _) => Ok(format),
            (None, Some(w)) if w > 0.0 => Ok(WireFormat::Json),
            (None, _) if named_a_wire_format => Err(()),
            (None, _) => Ok(WireFormat::Json),
        }
    }
}

/// Decode `bytes` in `format` and re-encode them as JSON.
///
/// # Errors
/// A message describing why the body could not be decoded.
pub fn to_json(format: WireFormat, bytes: &[u8]) -> Result<Vec<u8>, String> {
    if format == WireFormat::Json {
        return Ok(bytes.to_vec());
    }
    let value = decode_value(format, bytes)?;
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

/// Decode a body in `format` into a JSON value.
fn decode_value(format: WireFormat, bytes: &[u8]) -> Result<serde_json::Value, String> {
    match format {
        WireFormat::Json => serde_json::from_slice(bytes).map_err(|e| e.to_string()),
        #[cfg(feature = "format-cbor")]
        WireFormat::Cbor => ciborium::from_reader(bytes).map_err(|e| e.to_string()),
        #[cfg(feature = "format-msgpack")]
        WireFormat::MsgPack => rmp_serde::from_slice(bytes).map_err(|e| e.to_string()),
        #[allow(unreachable_patterns)]
        other => Err(format!("{other} is not compiled in")),
    }
}

/// Decode a JSON body and re-encode it in `format`.
///
/// # Errors
/// A message describing why the body could not be transcoded.
pub fn from_json(format: WireFormat, json: &[u8]) -> Result<Vec<u8>, String> {
    if format == WireFormat::Json {
        return Ok(json.to_vec());
    }
    let value: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    match format {
        #[cfg(feature = "format-cbor")]
        WireFormat::Cbor => {
            let mut out = Vec::new();
            ciborium::into_writer(&value, &mut out).map_err(|e| e.to_string())?;
            Ok(out)
        }
        #[cfg(feature = "format-msgpack")]
        WireFormat::MsgPack => rmp_serde::to_vec_named(&value).map_err(|e| e.to_string()),
        #[allow(unreachable_patterns)]
        other => {
            let _ = value;
            Err(format!("{other} is not compiled in"))
        }
    }
}

/// The largest request body the middleware will buffer to transcode, matching
/// the largest route limit (GraphML import).
const MAX_BODY: usize = 1024 * 1024 * 1024;

fn plain_error(status: StatusCode, message: String) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Middleware: transcode CBOR / MessagePack request bodies to JSON and JSON
/// responses to the format the client accepts. See the [module docs](self).
pub async fn negotiate(
    State(formats): State<Arc<WireFormats>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let accept = req
        .headers()
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Ok(response_format) = formats.response_format(accept.as_deref()) else {
        let offered: Vec<&str> = formats.enabled().iter().map(|f| f.media_type()).collect();
        return plain_error(
            StatusCode::NOT_ACCEPTABLE,
            format!(
                "none of the accepted types is offered; available: {}",
                offered.join(", ")
            ),
        );
    };

    let request_format = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| WireFormat::from_media_type(ct.split(';').next().unwrap_or("")));
    let req = match request_format {
        Some(format) if format != WireFormat::Json => {
            if !formats.is_enabled(format) {
                return plain_error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!("request body format {} is not enabled", format.media_type()),
                );
            }
            let (mut parts, body) = req.into_parts();
            let bytes = match to_bytes(body, MAX_BODY).await {
                Ok(b) => b,
                Err(e) => return plain_error(StatusCode::BAD_REQUEST, e.to_string()),
            };
            let json = match to_json(format, &bytes) {
                Ok(j) => j,
                Err(e) => {
                    return plain_error(
                        StatusCode::BAD_REQUEST,
                        format!("invalid {} body: {e}", format.media_type()),
                    )
                }
            };
            parts
                .headers
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            parts.headers.remove(CONTENT_LENGTH);
            Request::from_parts(parts, Body::from(json))
        }
        _ => req,
    };

    let mut resp = next.run(req).await;
    if formats.enabled().len() > 1 {
        resp.headers_mut()
            .append(VARY, HeaderValue::from_static("accept"));
    }
    if response_format == WireFormat::Json {
        return resp;
    }
    let is_json = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.split(';').next().unwrap_or("").trim() == "application/json");
    if !is_json {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    match from_json(response_format, &bytes) {
        Ok(encoded) => {
            parts.headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static(response_format.media_type()),
            );
            parts.headers.remove(CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(encoded))
        }
        // A handler that labelled a non-JSON body as JSON: hand it over as is
        // rather than fail the request.
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> WireFormats {
        WireFormats {
            enabled: WireFormat::ALL.to_vec(),
        }
    }

    #[test]
    fn parse_adds_json_and_rejects_unknown_names() {
        assert_eq!("".parse::<WireFormats>().unwrap(), WireFormats::default());
        assert_eq!(
            " JSON , ".parse::<WireFormats>().unwrap(),
            WireFormats::default()
        );
        assert_eq!(
            "xml".parse::<WireFormats>(),
            Err(FormatsError::Unknown("xml".into()))
        );
    }

    #[cfg(all(feature = "format-cbor", feature = "format-msgpack"))]
    #[test]
    fn parse_enables_compiled_formats() {
        let f: WireFormats = "msgpack,cbor,cbor".parse().unwrap();
        assert_eq!(
            f.enabled(),
            &[WireFormat::Json, WireFormat::MsgPack, WireFormat::Cbor]
        );
    }

    #[cfg(not(feature = "format-cbor"))]
    #[test]
    fn parse_rejects_formats_not_compiled_in() {
        assert_eq!(
            "cbor".parse::<WireFormats>(),
            Err(FormatsError::NotCompiled("cbor".into()))
        );
    }

    #[test]
    fn negotiation_follows_accept_weights() {
        let f = all();
        assert_eq!(f.response_format(None), Ok(WireFormat::Json));
        assert_eq!(f.response_format(Some("*/*")), Ok(WireFormat::Json));
        assert_eq!(f.response_format(Some("text/html")), Ok(WireFormat::Json));
        assert_eq!(
            f.response_format(Some("application/cbor")),
            Ok(WireFormat::Cbor)
        );
        assert_eq!(
            f.response_format(Some("application/cbor;q=0.5, application/x-msgpack")),
            Ok(WireFormat::MsgPack)
        );
        assert_eq!(
            f.response_format(Some("application/cbor, application/json")),
            Ok(WireFormat::Json),
            "equal weights prefer JSON"
        );
        assert_eq!(
            f.response_format(Some("application/cbor;q=0.2, */*;q=0.8")),
            Ok(WireFormat::Json)
        );
        assert_eq!(
            f.response_format(Some("application/cbor;q=0")),
            Err(()),
            "q=0 refuses the only named format"
        );
    }

    #[test]
    fn a_disabled_format_is_not_acceptable_unless_something_else_is() {
        let json_only = WireFormats::default();
        assert_eq!(json_only.response_format(Some("application/cbor")), Err(()));
        assert_eq!(
            json_only.response_format(Some("application/cbor, */*;q=0.1")),
            Ok(WireFormat::Json)
        );
        assert_eq!(
            json_only.response_format(Some("application/msgpack, application/json;q=0.5")),
            Ok(WireFormat::Json)
        );
    }

    /// drevo enables serde_json's `float_roundtrip`: without it the parser can
    /// land one ULP off, so a float sent as JSON (an embedding, a parameter)
    /// would be stored slightly changed, and CBOR could no longer shorten an
    /// `f32` value to 5 bytes.
    #[test]
    fn json_floats_parse_back_exactly() {
        let mut x = 0.123_f32;
        for _ in 0..10_000 {
            let v = f64::from(x);
            let text = serde_json::to_string(&v).unwrap();
            assert_eq!(
                serde_json::from_str::<f64>(&text).unwrap().to_bits(),
                v.to_bits(),
                "{text}"
            );
            x = x * 1.618 + 0.001;
            if x > 1e6 {
                x /= 1e7;
            }
        }
    }

    #[cfg(all(feature = "format-cbor", feature = "format-msgpack"))]
    #[test]
    fn transcoding_round_trips_through_every_format() {
        let json = br#"{"a":[1,-2,3.5,null,true,"s",{"b":[]}],"c":{}}"#;
        let value: serde_json::Value = serde_json::from_slice(json).unwrap();
        for format in WireFormat::ALL {
            let encoded = from_json(format, json).unwrap();
            let back = to_json(format, &encoded).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&back).unwrap(),
                value,
                "{format}"
            );
        }
    }

    #[cfg(feature = "format-cbor")]
    #[test]
    fn cbor_writes_f32_representable_floats_in_five_bytes() {
        let vector: Vec<f64> = (0..256).map(|i| f64::from(i as f32 * 0.123_f32)).collect();
        let json = serde_json::to_vec(&vector).unwrap();
        let cbor = from_json(WireFormat::Cbor, &json).unwrap();
        assert!(
            cbor.len() <= 3 + vector.len() * 5,
            "{} bytes for {} floats",
            cbor.len(),
            vector.len()
        );
    }
}
