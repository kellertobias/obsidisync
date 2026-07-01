use crate::password_auth::{PasswordAuth, PasswordAuthSession};
use anyhow::{anyhow, bail, Result};
use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use url::Url;

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub subject: String,
    pub user: String,
}

#[derive(Clone)]
pub enum AuthVerifier {
    Oidc(Arc<OidcVerifier>),
    Password(Arc<PasswordAuth>),
    StaticTokenForDev { token: String, user: String },
}

#[derive(Debug)]
pub struct OidcVerifier {
    issuer: String,
    audience: String,
    jwks_url: Option<String>,
    user_claim: String,
    jwks: RwLock<Option<JwkSet>>,
    http_client: reqwest::Client,
    allowed_algorithms: Vec<Algorithm>,
}

#[derive(Debug, Deserialize, Clone)]
struct Claims {
    sub: String,
    iss: String,
    aud: Audience,
    exp: usize,
    preferred_username: Option<String>,
    email: Option<String>,
    name: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct OpenIdConfiguration {
    jwks_uri: String,
}

impl AuthVerifier {
    pub fn oidc(
        issuer: String,
        audience: String,
        jwks_url: Option<String>,
        user_claim: String,
    ) -> Result<Self> {
        let issuer = validate_oidc_https_url(&issuer, "OIDC issuer")?;
        let jwks_url = jwks_url
            .map(|url| validate_oidc_https_url(&url, "OIDC JWKS URL"))
            .transpose()?;
        Ok(Self::Oidc(Arc::new(OidcVerifier {
            issuer,
            audience,
            jwks_url,
            user_claim,
            jwks: RwLock::new(None),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            allowed_algorithms: default_oidc_algorithms(),
        })))
    }

    pub fn password(user: String, data_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::password_with_setup_token(user, data_dir, None)
    }

    pub fn password_with_setup_token(
        user: String,
        data_dir: impl Into<PathBuf>,
        setup_token: Option<String>,
    ) -> Result<Self> {
        Ok(Self::Password(Arc::new(PasswordAuth::new(
            user,
            data_dir.into(),
            setup_token,
        )?)))
    }

    pub async fn password_is_configured(&self) -> Result<bool> {
        match self {
            AuthVerifier::Password(verifier) => verifier.is_configured().await,
            _ => bail!("password login is not enabled"),
        }
    }

    pub fn password_setup_token_is_required(&self) -> Result<bool> {
        match self {
            AuthVerifier::Password(verifier) => Ok(verifier.setup_token_is_required()),
            _ => bail!("password login is not enabled"),
        }
    }

    pub async fn setup_password(
        &self,
        username: &str,
        password: &str,
        setup_token: Option<&str>,
    ) -> Result<PasswordAuthSession> {
        match self {
            AuthVerifier::Password(verifier) => {
                verifier
                    .setup_password(username, password, setup_token)
                    .await
            }
            _ => bail!("password login is not enabled"),
        }
    }

    pub async fn login_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<PasswordAuthSession> {
        match self {
            AuthVerifier::Password(verifier) => verifier.login(username, password).await,
            _ => bail!("password login is not enabled"),
        }
    }

    pub async fn verify_headers(&self, headers: &HeaderMap) -> Result<AuthContext> {
        self.verify_headers_inner(headers)
            .await
            .map_err(|error| anyhow!("unauthorized: {error}"))
    }

    async fn verify_headers_inner(&self, headers: &HeaderMap) -> Result<AuthContext> {
        let header = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("missing authorization header"))?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| anyhow!("authorization header must use Bearer"))?;

        self.verify_bearer_token(token).await
    }

    pub async fn verify_bearer_token(&self, token: &str) -> Result<AuthContext> {
        match self {
            AuthVerifier::Oidc(verifier) => verifier.verify_token(token).await,
            AuthVerifier::Password(verifier) => {
                let user = verifier.verify_token(token).await?;
                Ok(AuthContext {
                    subject: user.clone(),
                    user,
                })
            }
            AuthVerifier::StaticTokenForDev {
                token: expected,
                user,
            } => {
                if !expected.is_empty() && !token.is_empty() && constant_time_eq(token, expected) {
                    let user = normalize_user_claim(user)?;
                    Ok(AuthContext {
                        subject: user.clone(),
                        user,
                    })
                } else {
                    bail!("invalid bearer token")
                }
            }
        }
    }
}

impl OidcVerifier {
    async fn verify_token(&self, token: &str) -> Result<AuthContext> {
        let header = decode_header(token)?;
        if !self.allowed_algorithms.contains(&header.alg) {
            bail!("OIDC token algorithm is not allowed");
        }
        let kid = header.kid.ok_or_else(|| anyhow!("OIDC token has no kid"))?;
        let jwks = self.jwks().await?;
        let jwk = jwks
            .keys
            .iter()
            .find(|candidate| candidate.common.key_id.as_deref() == Some(kid.as_str()))
            .ok_or_else(|| anyhow!("no JWKS key found for token kid"))?;

        let key = DecodingKey::from_jwk(jwk)?;
        let mut validation = Validation::new(header.alg);
        validation.algorithms = vec![header.alg];
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        let token_data = decode::<Claims>(token, &key, &validation)?;
        let claims = token_data.claims;
        validate_claims(&claims, &self.issuer, &self.audience)?;
        let user = select_user_claim(&claims, &self.user_claim);
        Ok(AuthContext {
            subject: claims.sub,
            user: normalize_user_claim(&user)?,
        })
    }

    async fn jwks(&self) -> Result<JwkSet> {
        if let Some(jwks) = self.jwks.read().await.clone() {
            return Ok(jwks);
        }
        let jwks_url = match &self.jwks_url {
            Some(value) => value.clone(),
            None => self.discover_jwks_uri().await?,
        };
        let jwks = self
            .http_client
            .get(&jwks_url)
            .send()
            .await?
            .error_for_status()?
            .json::<JwkSet>()
            .await?;
        *self.jwks.write().await = Some(jwks.clone());
        Ok(jwks)
    }

    async fn discover_jwks_uri(&self) -> Result<String> {
        let issuer = self.issuer.trim_end_matches('/');
        let configuration = self
            .http_client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await?
            .error_for_status()?
            .json::<OpenIdConfiguration>()
            .await?;
        validate_oidc_https_url(&configuration.jwks_uri, "OIDC JWKS URL")
    }
}

fn default_oidc_algorithms() -> Vec<Algorithm> {
    vec![
        Algorithm::RS256,
        Algorithm::RS384,
        Algorithm::RS512,
        Algorithm::PS256,
        Algorithm::PS384,
        Algorithm::PS512,
        Algorithm::ES256,
        Algorithm::ES384,
        Algorithm::EdDSA,
    ]
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let left_byte = *left.get(index).unwrap_or(&0);
        let right_byte = *right.get(index).unwrap_or(&0);
        diff |= (left_byte ^ right_byte) as usize;
    }
    diff == 0
}

fn validate_oidc_https_url(raw_url: &str, label: &str) -> Result<String> {
    let trimmed = raw_url.trim();
    let url = Url::parse(trimmed).map_err(|_| anyhow!("{label} must be a valid URL"))?;
    if url.scheme() == "https" || (url.scheme() == "http" && is_localhost(url.host_str())) {
        Ok(trimmed.trim_end_matches('/').to_string())
    } else {
        bail!("{label} must use HTTPS, except localhost during development");
    }
}

fn is_localhost(host: Option<&str>) -> bool {
    matches!(
        host.map(|value| value.trim_matches(['[', ']']).to_ascii_lowercase()),
        Some(value) if value == "localhost" || value == "127.0.0.1" || value == "::1"
    )
}

fn validate_claims(claims: &Claims, issuer: &str, audience: &str) -> Result<()> {
    if claims.iss != issuer {
        bail!("OIDC issuer mismatch");
    }
    let audience_ok = match &claims.aud {
        Audience::One(value) => value == audience,
        Audience::Many(values) => values.iter().any(|value| value == audience),
    };
    if !audience_ok {
        bail!("OIDC audience mismatch");
    }
    let _ = claims.exp;
    Ok(())
}

pub fn normalize_user_claim(input: &str) -> Result<String> {
    let normalized = input
        .split('@')
        .next()
        .unwrap_or(input)
        .trim()
        .to_ascii_lowercase()
        .replace(' ', "-");
    if normalized.is_empty()
        || !normalized
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        bail!("OIDC user claim cannot be used as a namespace");
    }
    Ok(normalized)
}

fn select_user_claim(claims: &Claims, user_claim: &str) -> String {
    let selected = match user_claim {
        "sub" => Some(claims.sub.clone()),
        "email" => claims.email.clone(),
        "name" => claims.name.clone(),
        "preferred_username" => claims.preferred_username.clone(),
        other => claims.extra.get(other).and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }),
    };

    selected
        .or_else(|| claims.preferred_username.clone())
        .or_else(|| claims.email.clone())
        .unwrap_or_else(|| claims.sub.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        let mut extra = HashMap::new();
        extra.insert("nickname".to_string(), Value::String("Ali".to_string()));
        Claims {
            sub: "subject-1".to_string(),
            iss: "issuer".to_string(),
            aud: Audience::One("aud".to_string()),
            exp: 9_999_999_999,
            preferred_username: Some("alice".to_string()),
            email: Some("alice@example.com".to_string()),
            name: Some("Alice Example".to_string()),
            extra,
        }
    }

    #[test]
    fn selects_configured_and_fallback_user_claims() {
        let claims = claims();
        assert_eq!(select_user_claim(&claims, "nickname"), "Ali");
        assert_eq!(select_user_claim(&claims, "email"), "alice@example.com");
        assert_eq!(select_user_claim(&claims, "missing"), "alice");
    }

    #[test]
    fn oidc_rejects_symmetric_algorithms_by_default() {
        let allowed = default_oidc_algorithms();
        assert!(!allowed.contains(&Algorithm::HS256));
        assert!(allowed.contains(&Algorithm::RS256));
        assert!(allowed.contains(&Algorithm::EdDSA));
    }

    #[tokio::test]
    async fn password_auth_sets_password_and_verifies_bearer_tokens() {
        let root = tempfile::tempdir().unwrap();
        let verifier =
            AuthVerifier::password("Alice@example.com".to_string(), root.path()).unwrap();

        assert!(!verifier.password_is_configured().await.unwrap());
        let session = verifier
            .setup_password("alice", "correct horse battery staple", None)
            .await
            .unwrap();
        assert_eq!(session.user, "alice");
        assert!(verifier.password_is_configured().await.unwrap());

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", session.access_token).parse().unwrap(),
        );
        let auth = verifier.verify_headers(&headers).await.unwrap();
        assert_eq!(auth.user, "alice");

        assert!(verifier
            .login_password("alice", "wrong password")
            .await
            .is_err());
        assert!(verifier
            .setup_password("alice", "another correct password", None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn password_setup_requires_configured_bootstrap_token() {
        let root = tempfile::tempdir().unwrap();
        let verifier = AuthVerifier::password_with_setup_token(
            "Alice@example.com".to_string(),
            root.path(),
            Some("setup-token-123456".to_string()),
        )
        .unwrap();

        assert!(verifier
            .setup_password("alice", "correct horse battery staple", None)
            .await
            .is_err());
        assert!(verifier
            .setup_password(
                "alice",
                "correct horse battery staple",
                Some("wrong-token-123456"),
            )
            .await
            .is_err());
        let session = verifier
            .setup_password(
                "alice",
                "correct horse battery staple",
                Some("setup-token-123456"),
            )
            .await
            .unwrap();
        assert_eq!(session.user, "alice");
    }

    #[tokio::test]
    async fn static_dev_token_uses_normalized_user_and_constant_time_check() {
        let verifier = AuthVerifier::StaticTokenForDev {
            token: "secret".to_string(),
            user: "Alice@example.com".to_string(),
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        let auth = verifier.verify_headers(&headers).await.unwrap();
        assert_eq!(auth.user, "alice");
        assert!(constant_time_eq("same", "same"));
        assert!(!constant_time_eq("same", "different"));

        let empty_verifier = AuthVerifier::StaticTokenForDev {
            token: String::new(),
            user: "alice".to_string(),
        };
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert!(empty_verifier.verify_headers(&headers).await.is_err());
    }

    #[tokio::test]
    async fn verify_headers_reports_every_failure_reason_as_unauthorized() {
        // http::ApiError maps status codes by sniffing the error message for the word
        // "unauthorized", so every verify_headers failure path — regardless of the
        // underlying reason (bad token, missing header, wrong scheme) — must be wrapped
        // in that word. Otherwise the client never sees a 401, never triggers its
        // refresh-token retry, and a routine token expiry surfaces as a hard failure.
        let verifier = AuthVerifier::StaticTokenForDev {
            token: "secret".to_string(),
            user: "alice".to_string(),
        };

        let mut wrong_token = HeaderMap::new();
        wrong_token.insert("authorization", "Bearer wrong".parse().unwrap());
        let error = verifier.verify_headers(&wrong_token).await.unwrap_err();
        assert!(error.to_string().contains("unauthorized"));

        let no_header = HeaderMap::new();
        let error = verifier.verify_headers(&no_header).await.unwrap_err();
        assert!(error.to_string().contains("unauthorized"));

        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert("authorization", "Basic secret".parse().unwrap());
        let error = verifier.verify_headers(&wrong_scheme).await.unwrap_err();
        assert!(error.to_string().contains("unauthorized"));
    }

    #[test]
    fn oidc_urls_require_https_except_localhost() {
        assert!(validate_oidc_https_url("https://issuer.example.com/", "OIDC issuer").is_ok());
        assert!(validate_oidc_https_url("http://localhost:8080", "OIDC issuer").is_ok());
        assert!(validate_oidc_https_url("http://issuer.example.com", "OIDC issuer").is_err());
    }
}
