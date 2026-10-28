//! OAuth2 token management for IMAP and SMTP XOAUTH2 authentication
//!
//! Provides a [`TokenManager`] that caches access tokens per account and
//! refreshes them before expiry. Also provides the XOAUTH2 SASL helper and
//! an [`async_imap::Authenticator`] implementation for IMAP XOAUTH2 login.
//!
//! # Supported Providers
//!
//! - **Google**: `https://oauth2.googleapis.com/token`
//! - **Microsoft**: `https://login.microsoftonline.com/common/oauth2/v2.0/token`
//!
//! # Configuration
//!
//! Per-account OAuth2 settings are loaded from environment variables:
//! ```text
//! MAIL_OAUTH2_<SEGMENT>_PROVIDER=google|microsoft
//! MAIL_OAUTH2_<SEGMENT>_CLIENT_ID=...
//! MAIL_OAUTH2_<SEGMENT>_CLIENT_SECRET=...
//! MAIL_OAUTH2_<SEGMENT>_REFRESH_TOKEN=...
//! ```
//!
//! # Refresh token rotation
//!
//! Providers may return a new refresh token on each refresh (Microsoft
//! always does). The newest one is kept in memory and used for the next
//! refresh. When `MAIL_OAUTH2_TOKEN_STORE` is set, it is also written to
//! that file so it survives a restart; see [`TokenStore`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::errors::{AppError, AppResult};

/// Margin before token expiry at which we proactively refresh (seconds).
///
/// Matches the proven pattern from `email-oauth2-proxy` (600s = 10 minutes).
const TOKEN_REFRESH_MARGIN_SECS: u64 = 600;

/// Default token lifetime when the provider omits `expires_in` (seconds).
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;

// ─── Provider ────────────────────────────────────────────────────────────────

/// Supported OAuth2 providers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2Provider {
    Google,
    Microsoft,
}

impl OAuth2Provider {
    /// Parse provider name from configuration value
    pub fn parse(value: &str) -> AppResult<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "google" | "gmail" => Ok(Self::Google),
            "microsoft" | "outlook" | "office365" => Ok(Self::Microsoft),
            other => Err(AppError::InvalidInput(format!(
                "unsupported OAuth2 provider '{other}'; expected 'google' or 'microsoft'"
            ))),
        }
    }

    /// Token endpoint URL for this provider
    pub fn token_url(&self) -> &'static str {
        match self {
            Self::Google => "https://oauth2.googleapis.com/token",
            Self::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }
}

// ─── Per-account OAuth2 config ───────────────────────────────────────────────

/// OAuth2 configuration for a single account
#[derive(Debug, Clone)]
pub struct OAuth2AccountConfig {
    pub provider: OAuth2Provider,
    pub client_id: String,
    pub client_secret: SecretString,
    pub refresh_token: SecretString,
}

// ─── Cached token ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS) < self.expires_at
    }
}

// ─── Token manager ───────────────────────────────────────────────────────────

/// Manages OAuth2 access tokens with caching and automatic refresh.
///
/// Thread-safe via `Arc<Mutex<...>>` on the cache. Designed to be shared
/// across all MCP tool handlers.
#[derive(Debug, Clone)]
pub struct TokenManager {
    /// OAuth2 configs keyed by account_id
    configs: Arc<HashMap<String, OAuth2AccountConfig>>,
    /// Cached tokens keyed by account_id
    cache: Arc<Mutex<HashMap<String, CachedToken>>>,
    /// Rotated refresh tokens keyed by account_id; overrides the configured
    /// token once the provider has issued a newer one
    rotated: Arc<Mutex<HashMap<String, SecretString>>>,
    /// Optional on-disk store for rotated refresh tokens
    store: Option<Arc<TokenStore>>,
    /// Store key namespace (`oauth2`, `graph`, `ews`), so the same account
    /// id in different managers does not share an entry
    kind: &'static str,
    /// HTTP client for token requests (reused across refreshes)
    http: reqwest::Client,
}

impl TokenManager {
    /// Create a token manager from OAuth2 account configs.
    ///
    /// With a `store`, rotated refresh tokens are persisted and reloaded at
    /// startup. `kind` namespaces the store entries. Entries saved from a different
    /// configured refresh token are ignored, so replacing the token in the
    /// environment always takes effect.
    pub fn new(
        configs: HashMap<String, OAuth2AccountConfig>,
        kind: &'static str,
        store: Option<Arc<TokenStore>>,
    ) -> Self {
        let mut rotated = HashMap::new();
        if let Some(store) = &store {
            let file = store.read();
            for (account_id, config) in &configs {
                let key = store_key(kind, account_id);
                if let Some(token) = file.lookup(&key, config.refresh_token.expose_secret()) {
                    tracing::info!(account_id, kind, "using stored OAuth2 refresh token");
                    rotated.insert(account_id.clone(), SecretString::new(token.into()));
                }
            }
        }
        Self {
            configs: Arc::new(configs),
            cache: Arc::new(Mutex::new(HashMap::new())),
            rotated: Arc::new(Mutex::new(rotated)),
            store,
            kind,
            http: reqwest::Client::new(),
        }
    }

    /// Check whether an account has OAuth2 configured.
    pub fn has_oauth2(&self, account_id: &str) -> bool {
        self.configs.contains_key(account_id)
    }

    /// Get a valid access token for the given account.
    ///
    /// Returns a cached token if it is still valid (with refresh margin),
    /// otherwise performs a refresh_token grant. Implements a two-strike
    /// retry: if the first refresh fails, waits 1 second and retries once.
    pub async fn get_access_token(&self, account_id: &str) -> AppResult<String> {
        // Check cache first
        {
            let cache = self.cache.lock().await;
            if let Some(cached) = cache.get(account_id)
                && cached.is_valid()
            {
                return Ok(cached.access_token.clone());
            }
        }

        let oauth2_config = self.configs.get(account_id).ok_or_else(|| {
            AppError::InvalidInput(format!(
                "no OAuth2 configuration for account '{account_id}'"
            ))
        })?;

        // Two-strike refresh
        match self.refresh_token(account_id, oauth2_config).await {
            Ok(token) => Ok(token),
            Err(first_err) => {
                tracing::warn!(
                    account_id,
                    "OAuth2 token refresh failed (attempt 1), retrying: {first_err}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.refresh_token(account_id, oauth2_config)
                    .await
                    .map_err(|e| {
                        AppError::AuthFailed(format!(
                            "OAuth2 token refresh failed for account '{account_id}' after 2 attempts: {e}"
                        ))
                    })
            }
        }
    }

    /// Perform a refresh_token grant and cache the result.
    ///
    /// For public clients (empty `client_secret`), the `client_secret` parameter
    /// is omitted from the token request per OAuth2 spec.
    async fn refresh_token(
        &self,
        account_id: &str,
        config: &OAuth2AccountConfig,
    ) -> AppResult<String> {
        let secret = config.client_secret.expose_secret();
        let is_public_client = secret.is_empty() || secret == "none" || secret == "public";

        let current_refresh = self
            .rotated
            .lock()
            .await
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| config.refresh_token.clone());

        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("client_id", config.client_id.as_str()),
            ("refresh_token", current_refresh.expose_secret()),
        ];
        if !is_public_client {
            params.push(("client_secret", secret));
        }

        let response = self
            .http
            .post(config.provider.token_url())
            .form(&params)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("OAuth2 token request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::AuthFailed(format!(
                "OAuth2 token endpoint returned {status}: {body}"
            )));
        }

        let token_response: TokenResponse = response.json().await.map_err(|e| {
            AppError::Internal(format!("failed to parse OAuth2 token response: {e}"))
        })?;

        let expires_in = token_response
            .expires_in
            .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
        let cached = CachedToken {
            access_token: token_response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        };

        self.cache
            .lock()
            .await
            .insert(account_id.to_owned(), cached);

        if let Some(new_refresh) = token_response.refresh_token.as_deref()
            && !new_refresh.is_empty()
            && new_refresh != current_refresh.expose_secret()
        {
            self.rotated.lock().await.insert(
                account_id.to_owned(),
                SecretString::new(new_refresh.to_owned().into()),
            );
            if let Some(store) = &self.store {
                store.save(
                    &store_key(self.kind, account_id),
                    config.refresh_token.expose_secret(),
                    new_refresh,
                );
            }
        }

        tracing::info!(
            account_id,
            expires_in_secs = expires_in,
            "OAuth2 access token refreshed"
        );

        Ok(token_response.access_token)
    }
}

/// OAuth2 token endpoint response
#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    /// Present when the provider rotates refresh tokens (Microsoft)
    refresh_token: Option<String>,
    // token_type and scope are present but unused
}

// ─── Refresh token store ─────────────────────────────────────────────────────

/// Store entry key: manager kind plus account id.
fn store_key(kind: &str, account_id: &str) -> String {
    format!("{kind}:{account_id}")
}

/// Hex SHA-256 of a configured refresh token. Stored next to each rotated
/// token so an entry is only used while the env token it came from is
/// unchanged; the env token itself is never written to the file.
fn fingerprint(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default)]
    tokens: BTreeMap<String, StoredToken>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredToken {
    /// Fingerprint of the configured refresh token this entry derives from
    source_sha256: String,
    refresh_token: String,
}

impl StoreFile {
    /// Stored refresh token for `key`, if it derives from `env_token`.
    fn lookup(&self, key: &str, env_token: &str) -> Option<String> {
        self.tokens
            .get(key)
            .filter(|t| t.source_sha256 == fingerprint(env_token))
            .map(|t| t.refresh_token.clone())
    }
}

/// JSON file holding rotated refresh tokens (`MAIL_OAUTH2_TOKEN_STORE`).
///
/// Failures are never fatal: an unreadable file is treated as empty and a
/// failed write is logged once, so the server keeps working with in-memory
/// rotation. Token values are never logged.
#[derive(Debug)]
pub struct TokenStore {
    path: PathBuf,
    /// Serializes read-modify-write cycles between token managers
    write_lock: std::sync::Mutex<()>,
    /// Whether a write failure was already logged
    write_warned: AtomicBool,
}

impl TokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: std::sync::Mutex::new(()),
            write_warned: AtomicBool::new(false),
        }
    }

    fn read(&self) -> StoreFile {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %self.path.display(), "ignoring unreadable OAuth2 token store: {e}");
                StoreFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), "cannot read OAuth2 token store: {e}");
                StoreFile::default()
            }
        }
    }

    /// Record `refresh_token` for `key`, derived from `env_token`.
    fn save(&self, key: &str, env_token: &str, refresh_token: &str) {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.read();
        file.tokens.insert(
            key.to_owned(),
            StoredToken {
                source_sha256: fingerprint(env_token),
                refresh_token: refresh_token.to_owned(),
            },
        );
        if let Err(e) = write_private(&self.path, &file) {
            if !self.write_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    path = %self.path.display(),
                    "cannot write OAuth2 token store, rotated tokens kept in memory only: {e}"
                );
            }
        } else {
            self.write_warned.store(false, Ordering::Relaxed);
        }
    }
}

/// Write `file` as JSON via a temp file and rename, readable by the owner only.
fn write_private(path: &Path, file: &StoreFile) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut handle = options.open(&tmp)?;
        handle.write_all(&json)?;
        handle.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ─── XOAUTH2 SASL ───────────────────────────────────────────────────────────

/// Build the XOAUTH2 SASL response string.
///
/// Format per [Google XOAUTH2](https://developers.google.com/gmail/imap/xoauth2-protocol):
/// ```text
/// user=<email>\x01auth=Bearer <token>\x01\x01
/// ```
pub fn xoauth2_sasl(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

// ─── IMAP Authenticator ─────────────────────────────────────────────────────

/// XOAUTH2 authenticator for `async_imap::Client::authenticate`.
///
/// Holds the pre-computed SASL response and returns it on the first
/// challenge from the server.
pub struct XOAuth2Authenticator {
    response: Vec<u8>,
}

impl XOAuth2Authenticator {
    /// Create a new authenticator with the given XOAUTH2 SASL string.
    pub fn new(user: &str, access_token: &str) -> Self {
        Self {
            response: xoauth2_sasl(user, access_token).into_bytes(),
        }
    }
}

impl async_imap::Authenticator for XOAuth2Authenticator {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // XOAUTH2 sends the full SASL response on the first (empty) challenge
        self.response.clone()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_sasl_format() {
        let sasl = xoauth2_sasl("user@gmail.com", "ya29.token123");
        assert_eq!(
            sasl,
            "user=user@gmail.com\x01auth=Bearer ya29.token123\x01\x01"
        );
    }

    #[test]
    fn xoauth2_authenticator_returns_sasl_bytes() {
        use async_imap::Authenticator;
        let mut auth = XOAuth2Authenticator::new("user@gmail.com", "token");
        let response = auth.process(b"");
        assert_eq!(
            response,
            b"user=user@gmail.com\x01auth=Bearer token\x01\x01"
        );
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        assert_eq!(
            OAuth2Provider::parse("google").unwrap(),
            OAuth2Provider::Google
        );
        assert_eq!(
            OAuth2Provider::parse("gmail").unwrap(),
            OAuth2Provider::Google
        );
        assert_eq!(
            OAuth2Provider::parse("GOOGLE").unwrap(),
            OAuth2Provider::Google
        );
        assert_eq!(
            OAuth2Provider::parse("microsoft").unwrap(),
            OAuth2Provider::Microsoft
        );
        assert_eq!(
            OAuth2Provider::parse("outlook").unwrap(),
            OAuth2Provider::Microsoft
        );
        assert_eq!(
            OAuth2Provider::parse("office365").unwrap(),
            OAuth2Provider::Microsoft
        );
    }

    #[test]
    fn provider_parse_rejects_unknown() {
        assert!(OAuth2Provider::parse("yahoo").is_err());
        assert!(OAuth2Provider::parse("").is_err());
    }

    #[test]
    fn provider_token_urls() {
        assert_eq!(
            OAuth2Provider::Google.token_url(),
            "https://oauth2.googleapis.com/token"
        );
        assert!(
            OAuth2Provider::Microsoft
                .token_url()
                .contains("login.microsoftonline.com")
        );
    }

    #[test]
    fn token_response_parses_optional_refresh_token() {
        let rotated: TokenResponse = serde_json::from_str(
            r#"{"access_token":"a","expires_in":3600,"refresh_token":"r2","token_type":"Bearer"}"#,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("r2"));

        let plain: TokenResponse =
            serde_json::from_str(r#"{"access_token":"a","expires_in":3600}"#).unwrap();
        assert!(plain.refresh_token.is_none());
    }

    #[test]
    fn store_entry_used_only_while_env_token_unchanged() {
        let dir = std::env::temp_dir().join(format!("mail-mcp-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = TokenStore::new(dir.join("tokens.json"));

        assert!(store.read().lookup("graph:work", "env-1").is_none());

        store.save("graph:work", "env-1", "rotated-1");
        let file = store.read();
        assert_eq!(
            file.lookup("graph:work", "env-1").as_deref(),
            Some("rotated-1")
        );
        // Same account under another manager kind is a separate entry
        assert!(file.lookup("ews:work", "env-1").is_none());
        // A new env token (re-authentication) wins over the stored one
        assert!(file.lookup("graph:work", "env-2").is_none());

        let raw = std::fs::read_to_string(dir.join("tokens.json")).unwrap();
        assert!(!raw.contains("env-1"), "env token must not be written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("tokens.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_failures_are_not_fatal() {
        let dir = std::env::temp_dir().join(format!("mail-mcp-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, b"not json").unwrap();
        assert!(TokenStore::new(corrupt).read().tokens.is_empty());

        let unwritable = TokenStore::new(dir.join("missing-dir").join("tokens.json"));
        unwritable.save("oauth2:default", "env", "rotated");
        assert!(unwritable.read().tokens.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_token_validity() {
        let valid = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS + 100),
        };
        assert!(valid.is_valid());

        let expired = CachedToken {
            access_token: "tok".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(TOKEN_REFRESH_MARGIN_SECS - 1),
        };
        assert!(!expired.is_valid());
    }
}
