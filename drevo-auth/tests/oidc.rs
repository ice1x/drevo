//! OIDC bearer tokens: a JWT from an identity provider, checked against the
//! provider's key set (RS256 or ES256) and its `iss`, `aud`, `exp` and `nbf`
//! claims, then mapped to a user name and roles.
//!
//! The tokens here are signed in the test with a locally made key pair: an
//! RSA key from `fixtures/test_rsa_2048.pk8` (made for these tests only) and
//! a P-256 key generated per run.
#![cfg(feature = "oidc")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use drevo_auth::oidc::{
    parse_role_map, ClaimMapping, JwkSet, JwksSource, OidcError, OidcPrincipal, TokenValidator,
    ValidatorConfig,
};
use drevo_auth::users::Clock;
use ring::rand::SystemRandom;
use ring::signature::{
    EcdsaKeyPair, KeyPair, RsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING, RSA_PKCS1_SHA256,
};

const ISSUER: &str = "https://login.example.test/realms/drevo";
const AUDIENCE: &str = "drevo";
/// 2026-10-17T00:00:00Z.
const NOW: i64 = 1_792_195_200;

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// The provider: one RSA and one EC signing key, and its key set.
struct Provider {
    rsa: RsaKeyPair,
    ec: EcdsaKeyPair,
}

impl Provider {
    fn new() -> Self {
        let rsa = RsaKeyPair::from_pkcs8(include_bytes!("fixtures/test_rsa_2048.pk8")).unwrap();
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let ec = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        Self { rsa, ec }
    }

    /// The JWKS document, with the RSA key as `rsa-1` and the EC key as `ec-1`.
    fn jwks(&self) -> String {
        self.jwks_with_ids("rsa-1", "ec-1")
    }

    fn jwks_with_ids(&self, rsa_kid: &str, ec_kid: &str) -> String {
        // The RSA key's public half, made with the fixture.
        let rsa: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/test_rsa_2048.public.json")).unwrap();
        let (n, e) = (rsa["n"].clone(), rsa["e"].clone());
        let point = self.ec.public_key().as_ref(); // 0x04 || x || y
        serde_json::json!({"keys": [
            {"kty": "RSA", "kid": rsa_kid, "use": "sig", "alg": "RS256", "n": n, "e": e},
            {"kty": "EC", "kid": ec_kid, "crv": "P-256", "x": b64url(&point[1..33]), "y": b64url(&point[33..65])},
            // Keys the validator does not use are skipped.
            {"kty": "oct", "kid": "hmac", "k": "c2VjcmV0"},
            {"kty": "RSA", "kid": "enc-1", "use": "enc", "n": n, "e": e}
        ]})
        .to_string()
    }

    fn sign(&self, alg: &str, kid: Option<&str>, claims: &serde_json::Value) -> String {
        let mut header = serde_json::json!({"alg": alg, "typ": "JWT"});
        if let Some(kid) = kid {
            header["kid"] = kid.into();
        }
        let input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let rng = SystemRandom::new();
        let signature = match alg {
            "RS256" => {
                let mut sig = vec![0u8; self.rsa.public().modulus_len()];
                self.rsa
                    .sign(&RSA_PKCS1_SHA256, &rng, input.as_bytes(), &mut sig)
                    .unwrap();
                sig
            }
            "ES256" => self
                .ec
                .sign(&rng, input.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            _ => b"not a signature".to_vec(),
        };
        format!("{input}.{}", b64url(&signature))
    }
}

fn claims() -> serde_json::Value {
    serde_json::json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "sub": "6f1c-ann",
        "preferred_username": "ann",
        "groups": ["/developers", "staff"],
        "exp": NOW + 300,
        "nbf": NOW - 10,
        "iat": NOW - 10,
    })
}

/// A key set that counts its fetches and can change between them.
struct Keys {
    documents: Mutex<Vec<String>>,
    fetches: AtomicUsize,
}

impl Keys {
    fn new(documents: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            documents: Mutex::new(documents),
            fetches: AtomicUsize::new(0),
        })
    }
}

impl JwksSource for Keys {
    fn fetch(&self) -> Result<JwkSet, OidcError> {
        let n = self.fetches.fetch_add(1, Ordering::SeqCst);
        let docs = self.documents.lock().unwrap();
        let doc = docs.get(n).or(docs.last()).unwrap();
        JwkSet::from_json(doc)
    }
}

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn mapping() -> ClaimMapping {
    ClaimMapping {
        username_claim: "preferred_username".into(),
        roles_claim: Some("groups".into()),
        role_map: parse_role_map("/developers=editor, staff=reader, admins=admin").unwrap(),
    }
}

fn validator_with(source: Arc<Keys>) -> (TokenValidator, Arc<FixedClock>) {
    let clock = Arc::new(FixedClock(AtomicI64::new(NOW * 1000)));
    let v = TokenValidator::new(ValidatorConfig::new(ISSUER, AUDIENCE), mapping(), source)
        .with_clock(clock.clone());
    (v, clock)
}

fn validator(p: &Provider) -> TokenValidator {
    validator_with(Keys::new(vec![p.jwks()])).0
}

// ── Valid tokens ─────────────────────────────────────────────────────

#[test]
fn an_rs256_token_names_the_user_and_maps_its_groups_to_roles() {
    let p = Provider::new();
    let token = p.sign("RS256", Some("rsa-1"), &claims());
    assert_eq!(
        validator(&p).validate(&token),
        Ok(OidcPrincipal {
            name: "ann".into(),
            roles: vec!["editor".into(), "reader".into()],
        })
    );
}

#[test]
fn an_es256_token_is_accepted() {
    let p = Provider::new();
    let token = p.sign("ES256", Some("ec-1"), &claims());
    assert_eq!(validator(&p).validate(&token).unwrap().name, "ann");
}

#[test]
fn the_audience_may_be_a_list() {
    let p = Provider::new();
    let mut c = claims();
    c["aud"] = serde_json::json!(["account", AUDIENCE]);
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert!(validator(&p).validate(&token).is_ok());
}

#[test]
fn a_token_without_kid_uses_the_only_key_of_its_kind() {
    let p = Provider::new();
    let token = p.sign("ES256", None, &claims());
    assert!(validator(&p).validate(&token).is_ok());
}

// ── Refused tokens ───────────────────────────────────────────────────

#[test]
fn an_expired_token_is_refused_past_the_leeway() {
    let p = Provider::new();
    let mut c = claims();
    c["exp"] = (NOW - 30).into();
    let token = p.sign("RS256", Some("rsa-1"), &c);
    // 30 s late is inside the default 60 s leeway.
    assert!(validator(&p).validate(&token).is_ok());
    c["exp"] = (NOW - 120).into();
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(validator(&p).validate(&token), Err(OidcError::Expired));
}

#[test]
fn a_token_not_yet_valid_is_refused() {
    let p = Provider::new();
    let mut c = claims();
    c["nbf"] = (NOW + 600).into();
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(validator(&p).validate(&token), Err(OidcError::NotYetValid));
}

#[test]
fn a_token_without_exp_is_refused() {
    let p = Provider::new();
    let mut c = claims();
    c.as_object_mut().unwrap().remove("exp");
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(
        validator(&p).validate(&token),
        Err(OidcError::MissingClaim("exp".into()))
    );
}

#[test]
fn the_wrong_audience_or_issuer_is_refused() {
    let p = Provider::new();
    let mut c = claims();
    c["aud"] = "another-app".into();
    assert_eq!(
        validator(&p).validate(&p.sign("RS256", Some("rsa-1"), &c)),
        Err(OidcError::WrongAudience)
    );
    let mut c = claims();
    c["iss"] = "https://evil.example.test".into();
    assert_eq!(
        validator(&p).validate(&p.sign("RS256", Some("rsa-1"), &c)),
        Err(OidcError::WrongIssuer)
    );
}

#[test]
fn a_bad_signature_is_refused() {
    let p = Provider::new();
    let token = p.sign("RS256", Some("rsa-1"), &claims());
    // Swap in other claims, keeping the signature.
    let mut c = claims();
    c["preferred_username"] = "admin".into();
    let forged = p.sign("RS256", Some("rsa-1"), &c);
    let parts: Vec<&str> = token.split('.').collect();
    let forged_parts: Vec<&str> = forged.split('.').collect();
    let tampered = format!("{}.{}.{}", parts[0], forged_parts[1], parts[2]);
    assert_eq!(
        validator(&p).validate(&tampered),
        Err(OidcError::BadSignature)
    );
    // Another provider's key.
    let other = Provider::new();
    let token = other.sign("ES256", Some("ec-1"), &claims());
    assert_eq!(validator(&p).validate(&token), Err(OidcError::BadSignature));
}

#[test]
fn unsigned_and_symmetric_tokens_are_refused() {
    let p = Provider::new();
    for alg in ["none", "HS256", "RS512"] {
        let token = p.sign(alg, Some("rsa-1"), &claims());
        assert_eq!(
            validator(&p).validate(&token),
            Err(OidcError::UnsupportedAlgorithm(alg.into())),
            "{alg}"
        );
    }
}

#[test]
fn a_token_must_match_its_keys_type() {
    let p = Provider::new();
    // An RS256 header naming the EC key.
    let token = p.sign("RS256", Some("ec-1"), &claims());
    assert_eq!(validator(&p).validate(&token), Err(OidcError::BadSignature));
}

#[test]
fn malformed_tokens_are_refused() {
    let p = Provider::new();
    let v = validator(&p);
    for token in ["", "abc", "a.b", "a.b.c.d", "!!!.###.$$$"] {
        assert!(
            matches!(v.validate(token), Err(OidcError::Malformed(_))),
            "{token}"
        );
    }
}

#[test]
fn a_token_without_the_username_claim_is_refused() {
    let p = Provider::new();
    let mut c = claims();
    c.as_object_mut().unwrap().remove("preferred_username");
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(
        validator(&p).validate(&token),
        Err(OidcError::MissingClaim("preferred_username".into()))
    );
}

// ── Keys ─────────────────────────────────────────────────────────────

#[test]
fn an_unknown_kid_refreshes_the_key_set_once() {
    let p = Provider::new();
    // The provider rotated: its first key set had other ids.
    let keys = Keys::new(vec![p.jwks_with_ids("old-rsa", "old-ec"), p.jwks()]);
    let (v, _) = validator_with(keys.clone());
    let token = p.sign("RS256", Some("rsa-1"), &claims());
    assert!(v.validate(&token).is_ok(), "found after a refresh");
    assert_eq!(keys.fetches.load(Ordering::SeqCst), 2);
    // A known key needs no fetch.
    assert!(v.validate(&token).is_ok());
    assert_eq!(keys.fetches.load(Ordering::SeqCst), 2);
}

#[test]
fn an_id_still_unknown_after_a_refresh_is_refused_and_refreshes_are_rate_limited() {
    let p = Provider::new();
    let keys = Keys::new(vec![p.jwks()]);
    let (v, clock) = validator_with(keys.clone());
    let token = p.sign("RS256", Some("nobody"), &claims());
    assert_eq!(
        v.validate(&token),
        Err(OidcError::UnknownKey(Some("nobody".into())))
    );
    let after_first = keys.fetches.load(Ordering::SeqCst);
    // A burst of unknown ids does not hammer the provider.
    for _ in 0..5 {
        let _ = v.validate(&token);
    }
    assert_eq!(keys.fetches.load(Ordering::SeqCst), after_first);
    // After the refresh interval it may look again.
    clock.0.fetch_add(120_000, Ordering::SeqCst);
    let _ = v.validate(&token);
    assert_eq!(keys.fetches.load(Ordering::SeqCst), after_first + 1);
}

#[test]
fn key_sets_skip_keys_they_cannot_use() {
    let p = Provider::new();
    let set = JwkSet::from_json(&p.jwks()).unwrap();
    assert_eq!(set.len(), 2, "the RSA and EC signing keys");
    assert!(JwkSet::from_json("not json").is_err());
    assert!(JwkSet::from_json(r#"{"keys": "nope"}"#).is_err());
}

// ── Claim mapping ────────────────────────────────────────────────────

#[test]
fn groups_map_to_roles_and_unmapped_groups_are_ignored() {
    let p = Provider::new();
    let mut c = claims();
    c["groups"] = serde_json::json!(["unknown", "admins", "staff", "staff"]);
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(
        validator(&p).validate(&token).unwrap().roles,
        ["admin", "reader"]
    );
    // A single string counts as one group; no groups claim, no roles.
    c["groups"] = "staff".into();
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert_eq!(validator(&p).validate(&token).unwrap().roles, ["reader"]);
    c.as_object_mut().unwrap().remove("groups");
    let token = p.sign("RS256", Some("rsa-1"), &c);
    assert!(validator(&p).validate(&token).unwrap().roles.is_empty());
}

#[test]
fn the_username_can_come_from_sub() {
    let p = Provider::new();
    let mut m = mapping();
    m.username_claim = "sub".into();
    let v = TokenValidator::new(
        ValidatorConfig::new(ISSUER, AUDIENCE),
        m,
        Keys::new(vec![p.jwks()]),
    )
    .with_clock(Arc::new(FixedClock(AtomicI64::new(NOW * 1000))));
    let token = p.sign("RS256", Some("rsa-1"), &claims());
    assert_eq!(v.validate(&token).unwrap().name, "6f1c-ann");
}

#[test]
fn role_maps_are_parsed_from_pairs() {
    let map = parse_role_map("admins=admin,devs=editor, devs=publisher ,").unwrap();
    let expected: BTreeMap<String, Vec<String>> = [
        ("admins".to_string(), vec!["admin".to_string()]),
        (
            "devs".to_string(),
            vec!["editor".to_string(), "publisher".to_string()],
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(map, expected);
    assert!(parse_role_map("admins").is_err());
    assert!(parse_role_map("=admin").is_err());
    assert!(parse_role_map("admins=").is_err());
    assert!(parse_role_map("").unwrap().is_empty());
}

#[test]
fn only_three_part_tokens_look_like_jwts() {
    assert!(TokenValidator::looks_like_jwt("a.b.c"));
    assert!(!TokenValidator::looks_like_jwt(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    ));
    assert!(!TokenValidator::looks_like_jwt("a.b"));
}

#[test]
fn errors_never_carry_the_token() {
    let p = Provider::new();
    let token = p.sign("RS256", Some("nobody"), &claims());
    let v = validator(&p);
    let message = v.validate(&token).unwrap_err().to_string();
    assert!(!message.contains(token.split('.').nth(2).unwrap()));
    assert!(!message.contains(token.split('.').nth(1).unwrap()));
}
