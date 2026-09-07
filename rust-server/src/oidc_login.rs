//! Browser login through the configured OIDC issuer (authorization code flow with PKCE).
//!
//! The Obsidian plugin uses the device-authorization grant, which needs no browser redirect.
//! Pages served by this server (the change feed, the Saber login flow) need a browser session,
//! so they send the user to the issuer's authorization endpoint and back to
//! `/login/oidc/callback`, exchange the code for an access token, and turn that token into the
//! same site session cookie password mode issues. The issuer must list
//! `{public base url}/login/oidc/callback` as a redirect URI of the OIDC client.

use crate::http::{
    escape_html, redirect_with_site_session, site_session_cookie_is_secure, AppState,
    PublicAuthConfig,
};
use crate::nextcloud::public_base_url;
use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING: usize = 500;
pub const CALLBACK_PATH: &str = "/login/oidc/callback";
pub const START_PATH: &str = "/login/oidc/start";

#[derive(Debug)]
pub struct OidcLoginStore {
    pending: Mutex<HashMap<String, PendingLogin>>,
    endpoints: Mutex<Option<Endpoints>>,
    http_client: reqwest::Client,
}

#[derive(Debug)]
struct PendingLogin {
    code_verifier: String,
    next: Option<String>,
    created_at: Instant,
}

#[derive(Debug, Clone, Deserialize)]
struct Endpoints {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl Default for OidcLoginStore {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(None),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
        }
    }
}

impl OidcLoginStore {
    async fn endpoints(&self, issuer: &str) -> Result<Endpoints> {
        if let Some(endpoints) = self.endpoints.lock().await.clone() {
            return Ok(endpoints);
        }
        let issuer = issuer.trim_end_matches('/');
        let endpoints: Endpoints = self
            .http_client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await
            .context("OIDC discovery request failed")?
            .error_for_status()
            .context("OIDC discovery returned an error")?
            .json()
            .await
            .context("OIDC discovery document is invalid")?;
        *self.endpoints.lock().await = Some(endpoints.clone());
        Ok(endpoints)
    }

    async fn begin(&self, next: Option<String>) -> Result<(String, String)> {
        let state = random_urlsafe()?;
        let code_verifier = random_urlsafe()?;
        let mut pending = self.pending.lock().await;
        let now = Instant::now();
        pending.retain(|_, login| now.duration_since(login.created_at) < PENDING_TTL);
        if pending.len() >= MAX_PENDING {
            bail!("too many pending logins");
        }
        pending.insert(
            state.clone(),
            PendingLogin {
                code_verifier: code_verifier.clone(),
                next,
                created_at: now,
            },
        );
        Ok((state, code_verifier))
    }

    async fn take(&self, state: &str) -> Option<(String, Option<String>)> {
        let mut pending = self.pending.lock().await;
        let login = pending.remove(state)?;
        if login.created_at.elapsed() >= PENDING_TTL {
            return None;
        }
        Some((login.code_verifier, login.next))
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(START_PATH, get(start))
        .route(CALLBACK_PATH, get(callback))
}

#[derive(Debug, Deserialize, Default)]
struct StartQuery {
    next: Option<String>,
}

/// Redirects the browser to the issuer. `next` must be a site-relative path.
async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StartQuery>,
) -> Response {
    match start_inner(&state, &headers, query.next.as_deref()).await {
        Ok(url) => Redirect::to(&url).into_response(),
        Err(error) => error_page("OIDC login unavailable", &error.to_string()),
    }
}

async fn start_inner(state: &AppState, headers: &HeaderMap, next: Option<&str>) -> Result<String> {
    let PublicAuthConfig::Oidc {
        issuer,
        client_id,
        scope,
        ..
    } = &state.public_auth
    else {
        bail!("OIDC login is not enabled on this server");
    };
    let endpoints = state.oidc_login.endpoints(issuer).await?;
    let next = crate::http::safe_next_path(next);
    let (login_state, code_verifier) = state.oidc_login.begin(next).await?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let redirect_uri = format!("{}{CALLBACK_PATH}", public_base_url(headers));
    let mut url = url::Url::parse(&endpoints.authorization_endpoint)
        .context("issuer authorization endpoint is not a valid URL")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", scope)
        .append_pair("state", &login_state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

#[derive(Debug, Deserialize, Default)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(error) = &query.error {
        let description = query.error_description.as_deref().unwrap_or("");
        return error_page(
            "Login failed",
            &format!("The identity provider reported: {error} {description}"),
        );
    }
    match callback_inner(&state, &headers, query).await {
        Ok(response) => response,
        Err(error) => error_page("Login failed", &format!("{error:#}")),
    }
}

async fn callback_inner(
    state: &AppState,
    headers: &HeaderMap,
    query: CallbackQuery,
) -> Result<Response> {
    let PublicAuthConfig::Oidc {
        issuer, client_id, ..
    } = &state.public_auth
    else {
        bail!("OIDC login is not enabled on this server");
    };
    let code = query
        .code
        .ok_or_else(|| anyhow!("missing authorization code"))?;
    let login_state = query.state.ok_or_else(|| anyhow!("missing state"))?;
    let (code_verifier, next) = state
        .oidc_login
        .take(&login_state)
        .await
        .ok_or_else(|| anyhow!("login state is unknown or expired; start the login again"))?;
    let endpoints = state.oidc_login.endpoints(issuer).await?;
    let redirect_uri = format!("{}{CALLBACK_PATH}", public_base_url(headers));
    let response = state
        .oidc_login
        .http_client
        .post(&endpoints.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", code_verifier.as_str()),
        ])
        .send()
        .await
        .context("token request failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token endpoint answered {status}: {}",
            body.chars().take(300).collect::<String>()
        );
    }
    let token: TokenResponse =
        serde_json::from_str(&body).context("token endpoint response is not valid JSON")?;
    let session = state
        .auth
        .login_oidc(&token.access_token)
        .await
        .context("the access token was not accepted by this server")?;
    Ok(redirect_with_site_session(
        next.as_deref().unwrap_or("/change-feed"),
        &session.access_token,
        site_session_cookie_is_secure(headers),
    ))
}

/// The path a page should send an unauthenticated browser to, so it comes back to `next`.
pub fn start_url(next: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    format!(
        "{START_PATH}?next={}",
        utf8_percent_encode(next, NON_ALPHANUMERIC)
    )
}

fn error_page(title: &str, message: &str) -> Response {
    (
        axum::http::StatusCode::BAD_GATEWAY,
        Html(format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title} - ObsidiSync</title><style>:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }} body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }} main {{ width: min(680px, calc(100vw - 32px)); padding: 2rem 0; }}</style></head><body><main><h1>{title}</h1><p>{message}</p></main></body></html>"#,
            title = escape_html(title),
            message = escape_html(message),
        )),
    )
        .into_response()
}

fn random_urlsafe() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("random generator failed: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_logins_are_single_use() {
        let store = OidcLoginStore::default();
        let (state, verifier) = store.begin(Some("/x".into())).await.unwrap();
        assert!(verifier.len() >= 43);
        let (taken, next) = store.take(&state).await.unwrap();
        assert_eq!(taken, verifier);
        assert_eq!(next.as_deref(), Some("/x"));
        assert!(store.take(&state).await.is_none());
    }

    #[test]
    fn start_url_encodes_next() {
        assert_eq!(
            start_url("/index.php/login/v2/flow/abc"),
            "/login/oidc/start?next=%2Findex%2Ephp%2Flogin%2Fv2%2Fflow%2Fabc"
        );
    }
}
