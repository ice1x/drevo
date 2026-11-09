//! The chat endpoint extraction runs with: `DREVO_EXTRACT_*` (each falling
//! back to its `DREVO_TEXT2CYPHER_*` twin, so one endpoint serves both), or
//! the setting saved through `POST /config/extract`.
//!
//! ```
//! use drevo::extract::settings::ExtractConfig;
//!
//! let env = |key: &str| match key {
//!     "DREVO_TEXT2CYPHER_UPSTREAM" => Some("http://localhost:11434/v1/chat/completions".to_string()),
//!     "DREVO_EXTRACT_MODEL" => Some("qwen2.5:14b-instruct".to_string()),
//!     _ => None,
//! };
//! let config = ExtractConfig::from_env(env).unwrap().unwrap();
//! assert_eq!(config.upstream, "http://localhost:11434/v1/chat/completions");
//! assert_eq!(config.model, "qwen2.5:14b-instruct");
//! assert_eq!(config.timeout_ms, 60_000);
//! ```

use std::sync::Arc;

use super::ExtractError;

/// The chat endpoint extraction calls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractConfig {
    /// The `chat/completions` URL (`http`/`https`).
    pub upstream: String,
    /// The bearer token, if the endpoint needs one.
    pub api_key: Option<String>,
    /// The model asked when a call names none.
    pub model: String,
    /// How long one call may take, in milliseconds.
    pub timeout_ms: u64,
}

/// The model when neither `DREVO_EXTRACT_MODEL` nor
/// `DREVO_TEXT2CYPHER_MODEL` is set.
pub(crate) const DEFAULT_MODEL: &str = "gpt-4o-mini";
/// The call timeout when `DREVO_EXTRACT_TIMEOUT_MS` is unset.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// The file under the data directory that keeps the setting saved at runtime.
pub(crate) const EXTRACT_CONFIG_FILE: &str = "extract_config.json";
/// The context the file is sealed under when the store is encrypted.
pub(crate) const EXTRACT_CONFIG_CONTEXT: &[u8] = b"drevo extract config";

impl ExtractConfig {
    /// Read the setting from a getter like [`std::env::var`]; `Ok(None)`
    /// when neither `DREVO_EXTRACT_UPSTREAM` nor `DREVO_TEXT2CYPHER_UPSTREAM`
    /// is set.
    ///
    /// | Variable | Falls back to | Default |
    /// |---|---|---|
    /// | `DREVO_EXTRACT_UPSTREAM` | `DREVO_TEXT2CYPHER_UPSTREAM` | (unset) |
    /// | `DREVO_EXTRACT_API_KEY` | `DREVO_TEXT2CYPHER_API_KEY`, only while the upstream is the text-to-Cypher one | (unset) |
    /// | `DREVO_EXTRACT_MODEL` | `DREVO_TEXT2CYPHER_MODEL` | `gpt-4o-mini` |
    /// | `DREVO_EXTRACT_TIMEOUT_MS` | — | `60000` |
    ///
    /// # Errors
    ///
    /// [`ExtractError::Invalid`] for an upstream that is blank or not
    /// `http`/`https`, or a timeout that is not a whole number above 0.
    pub fn from_env<F>(getter: F) -> Result<Option<Self>, ExtractError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let set = |key: &str| {
            getter(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let t2c_upstream = getter("DREVO_TEXT2CYPHER_UPSTREAM");
        let upstream = match getter("DREVO_EXTRACT_UPSTREAM").or_else(|| t2c_upstream.clone()) {
            None => return Ok(None),
            Some(u) => validate_upstream(&u)?,
        };
        let shares_t2c = t2c_upstream
            .as_deref()
            .is_some_and(|t| t.trim() == upstream);
        let api_key = set("DREVO_EXTRACT_API_KEY").or_else(|| {
            if shares_t2c {
                set("DREVO_TEXT2CYPHER_API_KEY")
            } else {
                None
            }
        });
        let model = set("DREVO_EXTRACT_MODEL")
            .or_else(|| set("DREVO_TEXT2CYPHER_MODEL"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let timeout_ms = match getter("DREVO_EXTRACT_TIMEOUT_MS") {
            None => DEFAULT_TIMEOUT_MS,
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| *ms > 0)
                .ok_or_else(|| {
                    ExtractError::Invalid(format!(
                        "DREVO_EXTRACT_TIMEOUT_MS must be a whole number of milliseconds above 0, \
                         got `{raw}`"
                    ))
                })?,
        };
        Ok(Some(Self {
            upstream,
            api_key,
            model,
            timeout_ms,
        }))
    }
}

/// Validate and normalise an upstream URL: trimmed, non-empty, `http`/`https`
/// only — an operator's choice, never a request's.
fn validate_upstream(raw: &str) -> Result<String, ExtractError> {
    let upstream = raw.trim().to_string();
    if upstream.is_empty() {
        return Err(ExtractError::Invalid(
            "the extraction upstream must not be empty".to_string(),
        ));
    }
    if !(upstream.starts_with("http://") || upstream.starts_with("https://")) {
        return Err(ExtractError::Invalid(format!(
            "unsupported scheme in `{upstream}` (expected http:// or https://)"
        )));
    }
    Ok(upstream)
}

/// A change to the setting, as `POST /config/extract` sends it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtractConfigUpdate {
    /// The `chat/completions` URL; required.
    pub upstream: String,
    /// A new bearer token; blank or absent keeps the stored one.
    #[serde(default)]
    pub api_key: Option<String>,
    /// The model; blank or absent takes the default.
    #[serde(default)]
    pub model: Option<String>,
    /// The call timeout in milliseconds; absent takes the default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// The setting as `GET /config/extract` reports it: whether a key is set,
/// never the key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtractConfigStatus {
    /// Whether an endpoint is set.
    pub configured: bool,
    /// The `chat/completions` URL.
    pub upstream: Option<String>,
    /// The model.
    pub model: Option<String>,
    /// Whether a bearer token is stored.
    pub api_key_set: bool,
    /// The call timeout in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// The extraction endpoint a server runs with: set at runtime through
/// `POST /config/extract`, kept in `<data dir>/extract_config.json`
/// (owner-only, sealed when the store is encrypted) and winning over
/// `DREVO_EXTRACT_*` at the next start. A change installs the endpoint the
/// `drevo.extract.*` procedures use at once.
#[derive(Debug)]
pub struct ExtractConfigStore {
    path: Option<std::path::PathBuf>,
    config: std::sync::RwLock<Option<ExtractConfig>>,
}

impl ExtractConfigStore {
    /// A store that is not persisted, starting from `config`.
    #[must_use]
    pub fn in_memory(config: Option<ExtractConfig>) -> Arc<Self> {
        Arc::new(Self {
            path: None,
            config: std::sync::RwLock::new(config),
        })
    }

    /// The store behind `path`: its file when there is one, otherwise
    /// `env_fallback`.
    #[must_use]
    pub fn load(path: std::path::PathBuf, env_fallback: Option<ExtractConfig>) -> Arc<Self> {
        let stored = std::fs::read(&path)
            .and_then(|bytes| crate::native_service::decode_sidecar(EXTRACT_CONFIG_CONTEXT, bytes));
        let config = match stored {
            Ok(bytes) => serde_json::from_slice::<ExtractConfig>(&bytes)
                .ok()
                .or(env_fallback),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "cannot read the extraction config; using the DREVO_EXTRACT_* settings"
                    );
                }
                env_fallback
            }
        };
        Arc::new(Self {
            path: Some(path),
            config: std::sync::RwLock::new(config),
        })
    }

    /// The current setting, key included (for the server's own use).
    #[must_use]
    pub fn snapshot(&self) -> Option<ExtractConfig> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The current setting without its key.
    #[must_use]
    pub fn status(&self) -> ExtractConfigStatus {
        match &*self.config.read().unwrap_or_else(|e| e.into_inner()) {
            Some(c) => ExtractConfigStatus {
                configured: true,
                upstream: Some(c.upstream.clone()),
                model: Some(c.model.clone()),
                api_key_set: c.api_key.is_some(),
                timeout_ms: Some(c.timeout_ms),
            },
            None => ExtractConfigStatus {
                configured: false,
                upstream: None,
                model: None,
                api_key_set: false,
                timeout_ms: None,
            },
        }
    }

    /// Apply `update`: validate it, keep the stored key when the field is
    /// blank, write the file, install the endpoint, and swap the setting in.
    /// The file is written first, so a disk failure changes nothing.
    ///
    /// # Errors
    ///
    /// [`ExtractError::Invalid`] for a bad URL or timeout, a file that cannot
    /// be written, or a client that cannot be built.
    pub fn apply(&self, update: ExtractConfigUpdate) -> Result<ExtractConfigStatus, ExtractError> {
        let upstream = validate_upstream(&update.upstream)?;
        let timeout_ms = match update.timeout_ms {
            Some(0) => {
                return Err(ExtractError::Invalid(
                    "timeout_ms must be above 0".to_string(),
                ))
            }
            Some(ms) => ms,
            None => DEFAULT_TIMEOUT_MS,
        };
        let api_key = match update.api_key.map(|s| s.trim().to_string()) {
            Some(k) if !k.is_empty() => Some(k),
            _ => self.snapshot().and_then(|c| c.api_key),
        };
        let model = update
            .model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let next = ExtractConfig {
            upstream,
            api_key,
            model,
            timeout_ms,
        };
        if let Some(path) = &self.path {
            persist(path, &next)?;
        }
        install_config(&next)?;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = Some(next);
        Ok(self.status())
    }
}

/// Write `config` to `path` as JSON, owner-only, atomically, sealed when the
/// store is encrypted.
fn persist(path: &std::path::Path, config: &ExtractConfig) -> Result<(), ExtractError> {
    fn write_atomic(
        tmp: &std::path::Path,
        path: &std::path::Path,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(tmp, bytes)?;
        }
        std::fs::rename(tmp, path)
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| ExtractError::Invalid(format!("cannot serialise the setting: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    crate::native_service::sidecar_bytes(EXTRACT_CONFIG_CONTEXT, json.into_bytes())
        .and_then(|bytes| write_atomic(&tmp, path, &bytes))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            ExtractError::Invalid(format!("cannot persist the extraction setting: {e}"))
        })
}

/// Install the endpoint `config` describes for the `drevo.extract.*`
/// procedures. Without the `embeddings-proxy` build feature there is no
/// HTTP client to install, and the procedures keep reporting "not
/// configured".
///
/// # Errors
///
/// [`ExtractError::Invalid`] when the client cannot be built.
#[cfg(feature = "embeddings-proxy")]
pub fn install_config(config: &ExtractConfig) -> Result<(), ExtractError> {
    let completer = crate::llm::SyncChatCompleter::spawn(
        crate::llm::ChatEndpoint {
            upstream: config.upstream.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            timeout: Some(std::time::Duration::from_millis(config.timeout_ms)),
        },
        "extraction",
        "drevo-extract",
    )
    .map_err(|e| ExtractError::Invalid(e.to_string()))?;
    super::install(Arc::new(completer));
    Ok(())
}

/// Without the `embeddings-proxy` build feature there is no HTTP client: the
/// setting is kept for a build that has one.
///
/// # Errors
///
/// Never.
#[cfg(not(feature = "embeddings-proxy"))]
pub fn install_config(config: &ExtractConfig) -> Result<(), ExtractError> {
    let _ = config;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    const T2C: &str = "http://localhost:11434/v1/chat/completions";
    const OWN: &str = "https://llm.example/v1/chat/completions";

    #[test]
    fn nothing_set_is_no_setting() {
        assert_eq!(ExtractConfig::from_env(get(&[])).unwrap(), None);
    }

    #[test]
    fn the_text_to_cypher_endpoint_is_the_default() {
        let c = ExtractConfig::from_env(get(&[
            ("DREVO_TEXT2CYPHER_UPSTREAM", T2C),
            ("DREVO_TEXT2CYPHER_API_KEY", "sk-t2c"),
            ("DREVO_TEXT2CYPHER_MODEL", "qwen"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            c,
            ExtractConfig {
                upstream: T2C.into(),
                api_key: Some("sk-t2c".into()),
                model: "qwen".into(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }
        );
    }

    #[test]
    fn extract_variables_win() {
        let c = ExtractConfig::from_env(get(&[
            ("DREVO_TEXT2CYPHER_UPSTREAM", T2C),
            ("DREVO_TEXT2CYPHER_MODEL", "qwen"),
            ("DREVO_EXTRACT_UPSTREAM", OWN),
            ("DREVO_EXTRACT_API_KEY", "sk-own"),
            ("DREVO_EXTRACT_MODEL", "gpt-4.1-mini"),
            ("DREVO_EXTRACT_TIMEOUT_MS", "5000"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            c,
            ExtractConfig {
                upstream: OWN.into(),
                api_key: Some("sk-own".into()),
                model: "gpt-4.1-mini".into(),
                timeout_ms: 5000,
            }
        );
    }

    #[test]
    fn the_text_to_cypher_key_is_not_sent_to_another_upstream() {
        let c = ExtractConfig::from_env(get(&[
            ("DREVO_TEXT2CYPHER_UPSTREAM", T2C),
            ("DREVO_TEXT2CYPHER_API_KEY", "sk-t2c"),
            ("DREVO_EXTRACT_UPSTREAM", OWN),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.api_key, None);
        assert_eq!(c.model, DEFAULT_MODEL);
    }

    #[test]
    fn a_bad_upstream_or_timeout_is_refused() {
        let e = ExtractConfig::from_env(get(&[("DREVO_EXTRACT_UPSTREAM", "ftp://x")])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid extraction setting: unsupported scheme in `ftp://x` (expected http:// or https://)"
        );
        let e = ExtractConfig::from_env(get(&[("DREVO_EXTRACT_UPSTREAM", " ")])).unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid extraction setting: the extraction upstream must not be empty"
        );
        for bad in ["0", "soon", "-5"] {
            let e = ExtractConfig::from_env(get(&[
                ("DREVO_EXTRACT_UPSTREAM", OWN),
                ("DREVO_EXTRACT_TIMEOUT_MS", bad),
            ]))
            .unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "invalid extraction setting: DREVO_EXTRACT_TIMEOUT_MS must be a whole number \
                     of milliseconds above 0, got `{bad}`"
                )
            );
        }
    }

    #[test]
    fn the_store_applies_keeps_the_key_and_reports_without_it() {
        let _serial = crate::extract::TEST_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let store = ExtractConfigStore::in_memory(None);
        assert!(!store.status().configured);
        let st = store
            .apply(ExtractConfigUpdate {
                upstream: OWN.into(),
                api_key: Some("sk-secret".into()),
                model: Some("m".into()),
                timeout_ms: Some(1234),
            })
            .unwrap();
        assert_eq!(
            st,
            ExtractConfigStatus {
                configured: true,
                upstream: Some(OWN.into()),
                model: Some("m".into()),
                api_key_set: true,
                timeout_ms: Some(1234),
            }
        );
        let st = store
            .apply(ExtractConfigUpdate {
                upstream: T2C.into(),
                api_key: Some("  ".into()),
                model: None,
                timeout_ms: None,
            })
            .unwrap();
        assert!(st.api_key_set, "a blank key keeps the stored one");
        assert_eq!(st.model.as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(st.timeout_ms, Some(DEFAULT_TIMEOUT_MS));
        assert_eq!(
            store.snapshot().unwrap().api_key.as_deref(),
            Some("sk-secret")
        );
        let e = store
            .apply(ExtractConfigUpdate {
                upstream: "nope".into(),
                api_key: None,
                model: None,
                timeout_ms: Some(0),
            })
            .unwrap_err();
        assert!(e.to_string().contains("unsupported scheme"), "{e}");
        let e = store
            .apply(ExtractConfigUpdate {
                upstream: OWN.into(),
                api_key: None,
                model: None,
                timeout_ms: Some(0),
            })
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "invalid extraction setting: timeout_ms must be above 0"
        );
        assert_eq!(
            store.snapshot().unwrap().upstream,
            T2C,
            "a refused change changes nothing"
        );
    }

    #[test]
    fn the_store_persists_and_the_file_wins_next_time() {
        let _serial = crate::extract::TEST_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EXTRACT_CONFIG_FILE);
        let store = ExtractConfigStore::load(path.clone(), None);
        store
            .apply(ExtractConfigUpdate {
                upstream: OWN.into(),
                api_key: Some("sk-p".into()),
                model: Some("m".into()),
                timeout_ms: None,
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let env = ExtractConfig {
            upstream: T2C.into(),
            api_key: None,
            model: "env".into(),
            timeout_ms: 1,
        };
        let again = ExtractConfigStore::load(path, Some(env.clone()));
        assert_eq!(again.snapshot().unwrap().upstream, OWN);
        let missing = ExtractConfigStore::load(dir.path().join("absent.json"), Some(env.clone()));
        assert_eq!(missing.snapshot(), Some(env));
    }
}
