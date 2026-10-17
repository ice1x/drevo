//! OIDC bearer tokens: logging in with a JWT from an identity provider
//! (Keycloak, Kanidm, Auth0, …) instead of a local password.
//!
//! A [`TokenValidator`] checks a token's signature against the provider's
//! key set (RS256 or ES256), then its `iss`, `aud`, `exp` and `nbf` claims
//! with a clock-skew allowance. It then names the user from a claim and maps
//! the user's groups to roles through a [`ClaimMapping`]. The result is an
//! [`OidcPrincipal`]: a user name and roles, with no local user record.
//!
//! The crate makes no network calls. A [`JwksSource`] supplies the key set,
//! and the validator caches its keys by `kid`. An unknown `kid` (the
//! provider rotated its keys) fetches the set again, at most once a minute.
//!
//! ```no_run
//! use std::sync::Arc;
//! use drevo_auth::oidc::{parse_role_map, ClaimMapping, JwkSet, StaticJwks, TokenValidator, ValidatorConfig};
//!
//! let jwks = JwkSet::from_json(r#"{"keys": []}"#).unwrap();
//! let validator = TokenValidator::new(
//!     ValidatorConfig::new("https://login.example.com/realms/acme", "drevo"),
//!     ClaimMapping {
//!         username_claim: "preferred_username".into(),
//!         roles_claim: Some("groups".into()),
//!         role_map: parse_role_map("admins=admin,devs=editor").unwrap(),
//!     },
//!     Arc::new(StaticJwks(jwks)),
//! );
//! let principal = validator.validate("eyJhbGciOi…").unwrap();
//! println!("{} has roles {:?}", principal.name, principal.roles);
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};

use crate::users::{Clock, SystemClock};

/// Why a token was refused. Messages never contain the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcError {
    /// Not a JWT: not three base64url parts, or not JSON inside.
    #[error("malformed token: {0}")]
    Malformed(String),
    /// Signed with an algorithm other than RS256 or ES256 (including `none`).
    #[error("unsupported signing algorithm `{0}`; RS256 and ES256 are accepted")]
    UnsupportedAlgorithm(String),
    /// No key in the provider's set has the token's `kid`.
    #[error("no key {} in the provider's key set", .0.as_deref().map_or("without an id".to_string(), |k| format!("`{k}`")))]
    UnknownKey(Option<String>),
    /// The signature does not verify with the key.
    #[error("the token's signature does not verify")]
    BadSignature,
    /// Past `exp`.
    #[error("the token has expired")]
    Expired,
    /// Before `nbf`.
    #[error("the token is not valid yet")]
    NotYetValid,
    /// `iss` is not the configured issuer.
    #[error("the token is from another issuer")]
    WrongIssuer,
    /// `aud` does not include the configured audience.
    #[error("the token is for another audience")]
    WrongAudience,
    /// A required claim is missing or not of the right type.
    #[error("the token has no `{0}` claim")]
    MissingClaim(String),
    /// The key set could not be fetched or read.
    #[error("cannot get the provider's keys: {0}")]
    Jwks(String),
}

/// One signing key of a key set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyMaterial {
    /// RSA modulus and exponent, big-endian.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// An uncompressed P-256 point: `0x04 || x || y`.
    EcP256 { point: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Jwk {
    kid: Option<String>,
    key: KeyMaterial,
}

/// A provider's signing keys, as published at its `jwks_uri`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JwkSet {
    keys: Vec<Jwk>,
}

impl JwkSet {
    /// Read a JWKS document. RSA and P-256 EC signing keys are kept; keys
    /// for encryption (`"use": "enc"`) and of other types are skipped.
    ///
    /// # Errors
    ///
    /// [`OidcError::Jwks`] when the document is not a key set.
    pub fn from_json(json: &str) -> Result<Self, OidcError> {
        let doc: serde_json::Value =
            serde_json::from_str(json).map_err(|e| OidcError::Jwks(e.to_string()))?;
        let keys = doc
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| OidcError::Jwks("no `keys` list".into()))?;
        let mut out = Vec::new();
        for k in keys {
            let field = |name: &str| k.get(name).and_then(serde_json::Value::as_str);
            if field("use").is_some_and(|u| u != "sig") {
                continue;
            }
            let bytes = |name: &str| field(name).and_then(decode_base64url);
            let key = match field("kty") {
                Some("RSA") => match (bytes("n"), bytes("e")) {
                    (Some(n), Some(e)) => KeyMaterial::Rsa { n, e },
                    _ => continue,
                },
                Some("EC") if field("crv") == Some("P-256") => match (bytes("x"), bytes("y")) {
                    (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => {
                        let mut point = Vec::with_capacity(65);
                        point.push(0x04);
                        point.extend_from_slice(&x);
                        point.extend_from_slice(&y);
                        KeyMaterial::EcP256 { point }
                    }
                    _ => continue,
                },
                _ => continue,
            };
            out.push(Jwk {
                kid: field("kid").map(str::to_string),
                key,
            });
        }
        Ok(Self { keys: out })
    }

    /// How many usable keys the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set holds no usable key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Where a validator gets the provider's key set. The crate does no HTTP:
/// a server fetches the provider's `jwks_uri` and returns the document.
pub trait JwksSource: Send + Sync {
    /// The current key set.
    ///
    /// # Errors
    ///
    /// [`OidcError::Jwks`] when it cannot be fetched or read.
    fn fetch(&self) -> Result<JwkSet, OidcError>;
}

/// A fixed key set.
#[derive(Debug, Clone)]
pub struct StaticJwks(pub JwkSet);

impl JwksSource for StaticJwks {
    fn fetch(&self) -> Result<JwkSet, OidcError> {
        Ok(self.0.clone())
    }
}

/// The provider a token must come from and the audience it must be for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorConfig {
    /// The expected `iss`, exactly.
    pub issuer: String,
    /// A value `aud` must hold (a string or one of a list).
    pub audience: String,
    /// How far clocks may disagree when checking `exp` and `nbf`.
    pub leeway: Duration,
    /// The shortest time between two fetches of the key set for unknown ids.
    pub refresh_interval: Duration,
}

impl ValidatorConfig {
    /// Tokens from `issuer` for `audience`, with 60 s of leeway and at most
    /// one key-set fetch a minute for unknown key ids.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            leeway: Duration::from_secs(60),
            refresh_interval: Duration::from_secs(60),
        }
    }
}

/// Which claims name the user and their roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimMapping {
    /// The claim holding the user name: `sub`, `preferred_username`, `email`…
    pub username_claim: String,
    /// The claim holding the user's groups (a list of strings, or one
    /// string), if any.
    pub roles_claim: Option<String>,
    /// Group → roles. Groups not listed give no role.
    pub role_map: BTreeMap<String, Vec<String>>,
}

impl Default for ClaimMapping {
    /// The user name from `sub`, groups from `groups`, and no roles mapped.
    fn default() -> Self {
        Self {
            username_claim: "sub".into(),
            roles_claim: Some("groups".into()),
            role_map: BTreeMap::new(),
        }
    }
}

/// Read a role map: comma-separated `group=role` pairs. A group may appear
/// more than once to get several roles: `admins=admin,devs=editor`.
///
/// # Errors
///
/// A message naming the pair that is not `group=role`.
pub fn parse_role_map(spec: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((group, role)) = pair.split_once('=') else {
            return Err(format!("`{pair}` is not group=role"));
        };
        let (group, role) = (group.trim(), role.trim());
        if group.is_empty() || role.is_empty() {
            return Err(format!("`{pair}` is not group=role"));
        }
        let roles = map.entry(group.to_string()).or_default();
        if !roles.iter().any(|r| r == role) {
            roles.push(role.to_string());
        }
    }
    Ok(map)
}

/// A user a provider's token vouches for: a name and roles, with no local
/// user record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcPrincipal {
    /// From [`ClaimMapping::username_claim`].
    pub name: String,
    /// The roles the user's groups map to, by name, without repeats.
    pub roles: Vec<String>,
}

/// Checks provider tokens; see the [module docs](self).
pub struct TokenValidator {
    config: ValidatorConfig,
    mapping: ClaimMapping,
    source: Arc<dyn JwksSource>,
    clock: Arc<dyn Clock>,
    keys: RwLock<Option<HashMap<Option<String>, Vec<KeyMaterial>>>>,
    /// When an unknown key id last made the validator fetch the set again.
    last_refresh_ms: Mutex<Option<i64>>,
}

impl std::fmt::Debug for TokenValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenValidator")
            .field("config", &self.config)
            .field("mapping", &self.mapping)
            .finish_non_exhaustive()
    }
}

impl TokenValidator {
    /// A validator for `config`'s provider, naming users by `mapping`, with
    /// keys from `source` (fetched on first use).
    #[must_use]
    pub fn new(
        config: ValidatorConfig,
        mapping: ClaimMapping,
        source: Arc<dyn JwksSource>,
    ) -> Self {
        Self {
            config,
            mapping,
            source,
            clock: Arc::new(SystemClock),
            keys: RwLock::new(None),
            last_refresh_ms: Mutex::new(None),
        }
    }

    /// Read the time from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Whether `token` has a JWT's shape (three dot-separated parts), as
    /// opposed to a session token.
    #[must_use]
    pub fn looks_like_jwt(token: &str) -> bool {
        token.split('.').count() == 3
    }

    /// Check `token` and return the user it names.
    ///
    /// # Errors
    ///
    /// An [`OidcError`] saying why the token is refused.
    pub fn validate(&self, token: &str) -> Result<OidcPrincipal, OidcError> {
        let parts: Vec<&str> = token.split('.').collect();
        let [header_b64, claims_b64, signature_b64] = parts.as_slice() else {
            return Err(OidcError::Malformed("a JWT has three parts".into()));
        };
        let header = decode_json(header_b64, "header")?;
        let claims = decode_json(claims_b64, "claims")?;
        let signature = decode_base64url(signature_b64)
            .ok_or_else(|| OidcError::Malformed("the signature is not base64url".into()))?;
        let alg = header
            .get("alg")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| OidcError::Malformed("no `alg` in the header".into()))?;
        if alg != "RS256" && alg != "ES256" {
            return Err(OidcError::UnsupportedAlgorithm(alg.to_string()));
        }
        let kid = header
            .get("kid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let signed = format!("{header_b64}.{claims_b64}");
        let key = self.key_for(&kid, alg)?;
        let verified = match (alg, &key) {
            ("RS256", KeyMaterial::Rsa { n, e }) => RsaPublicKeyComponents { n, e }
                .verify(
                    &signature::RSA_PKCS1_2048_8192_SHA256,
                    signed.as_bytes(),
                    &signature,
                )
                .is_ok(),
            ("ES256", KeyMaterial::EcP256 { point }) => {
                UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                    .verify(signed.as_bytes(), &signature)
                    .is_ok()
            }
            _ => false,
        };
        if !verified {
            return Err(OidcError::BadSignature);
        }
        self.check_claims(&claims)?;
        self.principal(&claims)
    }

    /// The key for `kid` of the type `alg` needs, fetching the key set when
    /// it was never fetched or (rate-limited) does not know `kid`.
    fn key_for(&self, kid: &Option<String>, alg: &str) -> Result<KeyMaterial, OidcError> {
        let pick = |keys: &HashMap<Option<String>, Vec<KeyMaterial>>| -> Option<KeyMaterial> {
            let fits = |k: &KeyMaterial| match alg {
                "RS256" => matches!(k, KeyMaterial::Rsa { .. }),
                _ => matches!(k, KeyMaterial::EcP256 { .. }),
            };
            match kid {
                // A named key is used as it is: a type mismatch fails the
                // signature check.
                Some(_) => keys.get(kid).and_then(|ks| ks.first().cloned()),
                // No `kid`: the only key of the right type, if there is one.
                None => {
                    let candidates: Vec<&KeyMaterial> =
                        keys.values().flatten().filter(|k| fits(k)).collect();
                    match candidates.as_slice() {
                        [only] => Some((*only).clone()),
                        _ => None,
                    }
                }
            }
        };
        let mut refreshed = false;
        loop {
            let fetched = {
                let guard = self.keys.read().unwrap_or_else(|p| p.into_inner());
                match guard.as_ref() {
                    Some(keys) => {
                        if let Some(k) = pick(keys) {
                            return Ok(k);
                        }
                        true
                    }
                    None => false,
                }
            };
            if fetched {
                // An unknown id: the provider may have rotated its keys. Look
                // again once, and not more than once per refresh interval.
                if refreshed || !self.may_refetch() {
                    return Err(OidcError::UnknownKey(kid.clone()));
                }
                refreshed = true;
                *self
                    .last_refresh_ms
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(self.clock.now_ms());
            }
            self.refetch()?;
        }
    }

    fn may_refetch(&self) -> bool {
        let last = *self
            .last_refresh_ms
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let interval = i64::try_from(self.config.refresh_interval.as_millis()).unwrap_or(i64::MAX);
        last.is_none_or(|t| self.clock.now_ms().saturating_sub(t) >= interval)
    }

    fn refetch(&self) -> Result<(), OidcError> {
        let set = self.source.fetch()?;
        let mut keys: HashMap<Option<String>, Vec<KeyMaterial>> = HashMap::new();
        for jwk in set.keys {
            keys.entry(jwk.kid).or_default().push(jwk.key);
        }
        *self.keys.write().unwrap_or_else(|p| p.into_inner()) = Some(keys);
        Ok(())
    }

    fn check_claims(&self, claims: &serde_json::Value) -> Result<(), OidcError> {
        let now = self.clock.now_ms().div_euclid(1000);
        let leeway = i64::try_from(self.config.leeway.as_secs()).unwrap_or(i64::MAX);
        if claims.get("iss").and_then(serde_json::Value::as_str) != Some(&self.config.issuer) {
            return Err(OidcError::WrongIssuer);
        }
        let audience_ok = match claims.get("aud") {
            Some(serde_json::Value::String(a)) => *a == self.config.audience,
            Some(serde_json::Value::Array(list)) => list
                .iter()
                .any(|a| a.as_str() == Some(self.config.audience.as_str())),
            _ => false,
        };
        if !audience_ok {
            return Err(OidcError::WrongAudience);
        }
        let exp = claims
            .get("exp")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| OidcError::MissingClaim("exp".into()))?;
        if now > exp.saturating_add(leeway) {
            return Err(OidcError::Expired);
        }
        if let Some(nbf) = claims.get("nbf").and_then(serde_json::Value::as_i64) {
            if now.saturating_add(leeway) < nbf {
                return Err(OidcError::NotYetValid);
            }
        }
        Ok(())
    }

    fn principal(&self, claims: &serde_json::Value) -> Result<OidcPrincipal, OidcError> {
        let name = claims
            .get(&self.mapping.username_claim)
            .and_then(serde_json::Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| OidcError::MissingClaim(self.mapping.username_claim.clone()))?;
        let groups: Vec<&str> = match self
            .mapping
            .roles_claim
            .as_ref()
            .and_then(|c| claims.get(c))
        {
            Some(serde_json::Value::String(g)) => vec![g.as_str()],
            Some(serde_json::Value::Array(list)) => {
                list.iter().filter_map(serde_json::Value::as_str).collect()
            }
            _ => Vec::new(),
        };
        let roles: BTreeSet<String> = groups
            .iter()
            .filter_map(|g| self.mapping.role_map.get(*g))
            .flatten()
            .cloned()
            .collect();
        Ok(OidcPrincipal {
            name: name.to_string(),
            roles: roles.into_iter().collect(),
        })
    }
}

fn decode_json(part: &str, what: &str) -> Result<serde_json::Value, OidcError> {
    let bytes = decode_base64url(part)
        .ok_or_else(|| OidcError::Malformed(format!("the {what} is not base64url")))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| OidcError::Malformed(format!("the {what} is not JSON")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(OidcError::Malformed(format!(
            "the {what} is not a JSON object"
        )))
    }
}

/// Unpadded (or padded) base64url, as JWTs and JWKs use.
fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = input.trim_end_matches('=').as_bytes();
    if bytes.is_empty() || bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= value(c)? << (18 - 6 * i);
        }
        let n = chunk.len() * 6 / 8;
        out.extend_from_slice(&acc.to_be_bytes()[1..=n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode_base64url;

    #[test]
    fn base64url_decodes_unpadded_input_and_refuses_the_rest() {
        assert_eq!(decode_base64url("aGk").unwrap(), b"hi");
        assert_eq!(decode_base64url("aGk=").unwrap(), b"hi");
        assert_eq!(decode_base64url("-_8").unwrap(), [0xfb, 0xff]);
        assert!(decode_base64url("a").is_none());
        assert!(decode_base64url("a+b/").is_none(), "standard alphabet");
        assert!(decode_base64url("").is_none());
    }
}
