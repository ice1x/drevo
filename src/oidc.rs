//! An identity provider's signing keys, fetched over HTTPS for the bearer
//! tokens the server accepts (`oidc` feature).
//!
//! [`HttpJwks`](crate::oidc::HttpJwks) reads the provider's discovery document,
//! `<issuer>/.well-known/openid-configuration`, then the key set at its
//! `jwks_uri`. A [`TokenValidator`](drevo_auth::oidc::TokenValidator) calls it
//! on first use and again when a token names a key it does not have, so a
//! provider's key rotation needs no restart.
//!
//! ```no_run
//! use std::sync::Arc;
//! use drevo::oidc::HttpJwks;
//! use drevo_auth::oidc::{ClaimMapping, TokenValidator, ValidatorConfig};
//!
//! let issuer = "https://login.example.com/realms/drevo";
//! let validator = TokenValidator::new(
//!     ValidatorConfig::new(issuer, "drevo"),
//!     ClaimMapping::default(),
//!     Arc::new(HttpJwks::new(issuer).unwrap()),
//! );
//! ```
//!
//! The provider and its key set must be served over HTTPS, except from this
//! machine (`localhost`, `127.0.0.1`, `::1`). Redirects are not followed, a
//! request gives up after ten seconds, and a document over 1 MiB is refused.

use std::time::Duration;

use drevo_auth::oidc::{JwkSet, JwksSource, OidcError};
use reqwest::Url;

/// The largest discovery document or key set read.
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A provider's key set, found through its discovery document.
#[derive(Debug, Clone)]
pub struct HttpJwks {
    issuer: String,
    discovery: Url,
}

impl HttpJwks {
    /// The key set of the provider at `issuer`, the exact `iss` of its tokens.
    ///
    /// # Errors
    ///
    /// A message when `issuer` is not an `https://` URL, or `http://` on this
    /// machine.
    pub fn new(issuer: &str) -> Result<Self, String> {
        let base = checked_url(issuer)?;
        let discovery = Url::parse(&format!(
            "{}/.well-known/openid-configuration",
            base.as_str().trim_end_matches('/')
        ))
        .map_err(|e| format!("issuer `{issuer}`: {e}"))?;
        Ok(Self {
            issuer: issuer.to_string(),
            discovery,
        })
    }

    fn fetch_document(&self) -> Result<JwkSet, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("cannot make an HTTP client: {e}"))?;
        // The caller may be a worker of the server's runtime, so the requests
        // run on a thread of their own with a runtime of their own.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("cannot start an HTTP runtime: {e}"))?;
                    runtime.block_on(async {
                        let discovery: serde_json::Value =
                            serde_json::from_slice(&get(&client, &self.discovery).await?)
                                .map_err(|e| format!("{}: {e}", self.discovery))?;
                        let named = discovery.get("issuer").and_then(|v| v.as_str());
                        if named != Some(self.issuer.as_str()) {
                            return Err(format!(
                                "{} names the issuer {}, not {}",
                                self.discovery,
                                named.unwrap_or("(none)"),
                                self.issuer
                            ));
                        }
                        let jwks_uri = discovery
                            .get("jwks_uri")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| format!("{} has no jwks_uri", self.discovery))?;
                        let jwks_uri = checked_url(jwks_uri)?;
                        let body = get(&client, &jwks_uri).await?;
                        let text =
                            std::str::from_utf8(&body).map_err(|e| format!("{jwks_uri}: {e}"))?;
                        JwkSet::from_json(text).map_err(|e| format!("{jwks_uri}: {e}"))
                    })
                })
                .join()
                .unwrap_or_else(|_| Err("the key-set fetch failed".to_string()))
        })
    }
}

impl JwksSource for HttpJwks {
    fn fetch(&self) -> Result<JwkSet, OidcError> {
        self.fetch_document().map_err(OidcError::Jwks)
    }
}

/// `url` parsed, when it is `https://`, or `http://` on this machine.
fn checked_url(url: &str) -> Result<Url, String> {
    let parsed = Url::parse(url).map_err(|e| format!("`{url}` is not a URL: {e}"))?;
    let host = parsed.host_str().unwrap_or("");
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match parsed.scheme() {
        "https" if !host.is_empty() => Ok(parsed),
        "http" if local => Ok(parsed),
        _ => Err(format!(
            "`{url}` must be an https:// URL (http:// only on this machine)"
        )),
    }
}

/// The body at `url`, refused unless it answers 200 within the size limit.
async fn get(client: &reqwest::Client, url: &Url) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url.clone())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{url}: {e}"))? {
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(format!("{url}: larger than {MAX_DOCUMENT_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
