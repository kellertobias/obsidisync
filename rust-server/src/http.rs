use crate::auth::AuthVerifier;
use crate::auth_throttle::AuthThrottle;
use crate::device_passwords::{
    CreateDevicePasswordRequest, CreatedDevicePassword, DevicePasswordEntry, DevicePasswordStore,
};
use crate::nextcloud::LoginFlowStore;
use crate::oidc_login::OidcLoginStore;
use crate::protocol::*;
use crate::saber::push::{TabletPusher, DEFAULT_PUSH_DELAY};
use crate::saber::sync::{SaberRenderer, DEFAULT_RENDER_DELAY};
use crate::vault::VaultService;
use axum::extract::{DefaultBodyLimit, Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

const SITE_SESSION_COOKIE: &str = "obsidisync_session";
const SITE_SESSION_COOKIE_MAX_AGE_SECONDS: u64 = 30 * 24 * 60 * 60;
const SERVER_API_VERSION: u32 = 1;
const MIN_CLIENT_API_VERSION: u32 = 1;
/// Optional capabilities advertised to clients. Older plugins ignore the list; newer plugins
/// hide or explain features that the server they talk to does not have yet.
const SERVER_FEATURES: &[&str] = &[
    "webdavDevicePasswords",
    "syncFileReferences",
    "saberNextcloud",
];
/// PDFs exported from note-taking tablets are routinely larger than the JSON sync payload limit.
pub const DEFAULT_WEBDAV_MAX_BODY_BYTES: usize = 200 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub vaults: VaultService,
    pub auth: AuthVerifier,
    pub public_auth: PublicAuthConfig,
    pub device_passwords: Arc<DevicePasswordStore>,
    pub webdav_throttle: Arc<AuthThrottle>,
    /// Largest single WebDAV upload. Enforced while streaming the body to disk.
    pub webdav_max_body_bytes: usize,
    /// Renders Saber uploads to PDFs in the background.
    pub saber: Arc<SaberRenderer>,
    /// Pushes `#tablet`-tagged PDFs into Saber in the background.
    pub tablet: Arc<TabletPusher>,
    /// Pending Nextcloud Login Flow v2 sessions started by the Saber app.
    pub login_flows: Arc<LoginFlowStore>,
    /// Pending browser logins through the OIDC issuer.
    pub oidc_login: Arc<OidcLoginStore>,
}

impl AppState {
    pub fn new(vaults: VaultService, auth: AuthVerifier, public_auth: PublicAuthConfig) -> Self {
        let device_passwords = Arc::new(DevicePasswordStore::new(vaults.data_dir.clone()));
        let saber = SaberRenderer::new(vaults.clone(), DEFAULT_RENDER_DELAY);
        let tablet = TabletPusher::new(
            vaults.clone(),
            Arc::clone(&device_passwords),
            DEFAULT_PUSH_DELAY,
        );
        Self {
            vaults,
            auth,
            public_auth,
            device_passwords,
            webdav_throttle: Arc::new(AuthThrottle::new()),
            webdav_max_body_bytes: DEFAULT_WEBDAV_MAX_BODY_BYTES,
            saber,
            tablet,
            login_flows: Arc::new(LoginFlowStore::default()),
            oidc_login: Arc::new(OidcLoginStore::default()),
        }
    }

    /// Changes how long the Saber renderer waits for related uploads before rendering.
    pub fn with_saber_render_delay(mut self, delay: std::time::Duration) -> Self {
        self.saber = SaberRenderer::new(self.vaults.clone(), delay);
        self.tablet = TabletPusher::new(
            self.vaults.clone(),
            Arc::clone(&self.device_passwords),
            delay,
        );
        self
    }
}

#[derive(Debug)]
pub struct ApiError(anyhow::Error);

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub path: Option<String>,
}

#[derive(Clone, Debug)]
pub enum PublicAuthConfig {
    Password,
    Oidc {
        issuer: String,
        client_id: String,
        scope: String,
        audience: Option<String>,
    },
    Token,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum PublicAuthConfigResponse {
    Password {
        #[serde(rename = "passwordConfigured")]
        password_configured: bool,
        #[serde(rename = "setupTokenRequired")]
        setup_token_required: bool,
    },
    Oidc {
        issuer: String,
        #[serde(rename = "clientId")]
        client_id: String,
        scope: String,
        audience: Option<String>,
    },
    Token,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthSessionResponse {
    user: String,
    subject: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerInfoResponse {
    name: &'static str,
    version: &'static str,
    api_version: u32,
    min_client_api_version: u32,
    features: &'static [&'static str],
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        tracing::warn!(error = %message, "request failed");
        let status = if message.contains("unauthorized")
            || message.contains("authorization")
            || message.contains("OIDC")
            || message.contains("bearer")
            || message.contains("refresh token")
            || message.contains("invalid username or password")
            || message.contains("password is not set")
        {
            StatusCode::UNAUTHORIZED
        } else if message.contains("forbidden") {
            StatusCode::FORBIDDEN
        } else if message.contains("not found") || message.contains("Unknown vault") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (
            status,
            Json(ApiErrorBody {
                error: public_error_message(status, &message),
            }),
        )
            .into_response()
    }
}

pub fn router(state: AppState, max_body_bytes: usize, allowed_origins: Vec<String>) -> Router {
    router_with_webdav_limit(
        state,
        max_body_bytes,
        max_body_bytes.max(DEFAULT_WEBDAV_MAX_BODY_BYTES),
        allowed_origins,
    )
}

pub fn router_with_webdav_limit(
    mut state: AppState,
    max_body_bytes: usize,
    webdav_max_body_bytes: usize,
    allowed_origins: Vec<String>,
) -> Router {
    state.webdav_max_body_bytes = webdav_max_body_bytes;
    // WebDAV reads the raw request body itself (streamed to disk), so its size limit is
    // enforced in the handler rather than through `DefaultBodyLimit`.
    let webdav = Router::new()
        .route("/dav", any(crate::webdav::handle))
        .route("/dav/", any(crate::webdav::handle))
        .route("/dav/*path", any(crate::webdav::handle));
    let router = Router::new()
        .route("/", get(home_page))
        .route("/login", get(password_page).post(password_form))
        .route("/change-feed", get(change_feed_page))
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "ok": true })) }),
        )
        .route("/v1/server/info", get(server_info))
        .route("/v1/auth/config", get(auth_config))
        .route("/v1/auth/session", get(auth_session))
        .route("/v1/auth/session/refresh", post(refresh_session))
        .route("/v1/auth/oidc/login", post(login_oidc))
        .route("/v1/auth/password/setup", post(setup_password))
        .route("/v1/auth/password/login", post(login_password))
        .route("/v1/users/:user/feed", get(feed))
        .route("/v1/users/:user/vaults/:vault/register", post(register))
        .route("/v1/users/:user/vaults/:vault/uploads", post(init_upload))
        .route(
            "/v1/users/:user/vaults/:vault/uploads/:upload/chunk",
            post(upload_chunk),
        )
        .route(
            "/v1/users/:user/vaults/:vault/uploads/:upload/complete",
            post(complete_upload),
        )
        .route("/v1/users/:user/vaults/:vault/sync", post(sync))
        .route("/v1/users/:user/vaults/:vault/history", get(history))
        .route("/v1/users/:user/vaults/:vault/file", get(file_at_version))
        .route("/v1/users/:user/vaults/:vault/blob", get(blob_at_version))
        .route("/v1/users/:user/vaults/:vault/resolve", post(resolve))
        .route("/v1/users/:user/vaults/:vault/devices", get(devices))
        .route(
            "/v1/users/:user/vaults/:vault/conflicts",
            get(pending_conflicts),
        )
        .route(
            "/v1/users/:user/vaults/:vault/files/device-versions",
            get(device_versions),
        )
        .route(
            "/v1/users/:user/vaults/:vault/files/version-metadata",
            post(set_version_metadata),
        )
        .route(
            "/v1/users/:user/vaults/:vault/device-passwords",
            get(list_device_passwords).post(create_device_password),
        )
        .route(
            "/v1/users/:user/vaults/:vault/device-passwords/:id",
            delete(revoke_device_password),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .merge(webdav)
        .merge(crate::nextcloud::router())
        .merge(crate::oidc_login::router())
        .with_state(Arc::new(state));

    apply_cors(router, allowed_origins)
}

async fn home_page(State(state): State<Arc<AppState>>) -> Result<Html<String>, ApiError> {
    Ok(Html(render_home_page(&state.public_auth)))
}

async fn server_info() -> Json<ServerInfoResponse> {
    Json(ServerInfoResponse {
        name: "obsidisync-server",
        version: env!("CARGO_PKG_VERSION"),
        api_version: SERVER_API_VERSION,
        min_client_api_version: MIN_CLIENT_API_VERSION,
        features: SERVER_FEATURES,
    })
}

fn apply_cors(router: Router, allowed_origins: Vec<String>) -> Router {
    if allowed_origins.is_empty() {
        return router;
    }

    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    if allowed_origins.iter().any(|origin| origin == "*") {
        router.layer(layer.allow_origin(Any))
    } else {
        let origins: Vec<HeaderValue> = allowed_origins
            .into_iter()
            .filter_map(|origin| HeaderValue::from_str(origin.trim()).ok())
            .collect();
        if origins.is_empty() {
            router
        } else {
            router.layer(layer.allow_origin(origins))
        }
    }
}

fn public_error_message(status: StatusCode, message: &str) -> String {
    match status {
        StatusCode::UNAUTHORIZED => "unauthorized".to_string(),
        StatusCode::FORBIDDEN => "forbidden".to_string(),
        StatusCode::NOT_FOUND => "not found".to_string(),
        _ if is_public_client_error(message) => message.to_string(),
        _ => "request failed".to_string(),
    }
}

fn is_public_client_error(message: &str) -> bool {
    message.starts_with("invalid ")
        || message.starts_with("unsafe vault path")
        || message.starts_with("local git remotes are disabled")
        || message.starts_with("git remote ")
        || message.starts_with("OIDC user claim cannot be used")
        || message.starts_with("password must")
        || message.starts_with("password setup token is required")
        || message.starts_with("password is already set")
        || message.starts_with("password confirmation")
        || message.starts_with("invalid device password")
        || message.starts_with("invalid request")
        || message.starts_with("invalid vault")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordAuthRequest {
    username: String,
    password: String,
    #[serde(default, rename = "setupToken")]
    setup_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PasswordLoginForm {
    username: String,
    password: String,
    password_confirm: Option<String>,
    setup_token: Option<String>,
    /// Site-relative path to continue to after login (for example a Saber login flow page).
    next: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LoginPageQuery {
    next: Option<String>,
}

/// Only same-site paths are followed after login, never absolute URLs.
pub(crate) fn safe_next_path(next: Option<&str>) -> Option<String> {
    let next = next?.trim();
    if next.starts_with('/') && !next.starts_with("//") && !next.contains(['\r', '\n']) {
        Some(next.to_string())
    } else {
        None
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OidcLoginRequest {
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshSessionRequest {
    refresh_token: String,
}

async fn password_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LoginPageQuery>,
) -> Result<Response, ApiError> {
    if matches!(state.public_auth, PublicAuthConfig::Oidc { .. }) {
        let next =
            safe_next_path(query.next.as_deref()).unwrap_or_else(|| "/change-feed".to_string());
        return Ok(
            axum::response::Redirect::to(&crate::oidc_login::start_url(&next)).into_response(),
        );
    }
    if !matches!(state.public_auth, PublicAuthConfig::Password) {
        return Ok(Html(render_home_page(&state.public_auth)).into_response());
    }
    let configured = state.auth.password_is_configured().await?;
    let setup_token_required = state.auth.password_setup_token_is_required()?;
    Ok(Html(render_password_page(
        configured,
        setup_token_required,
        None,
        None,
        &[],
        safe_next_path(query.next.as_deref()).as_deref(),
    ))
    .into_response())
}

async fn password_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<PasswordLoginForm>,
) -> Result<Response, ApiError> {
    if !matches!(state.public_auth, PublicAuthConfig::Password) {
        return Err(ApiError(anyhow::anyhow!("password login is not enabled")));
    }
    let configured = state.auth.password_is_configured().await?;
    let result = if configured {
        state
            .auth
            .login_password(&form.username, &form.password)
            .await
    } else if form.password_confirm.as_deref() != Some(form.password.as_str()) {
        Err(anyhow::anyhow!("password confirmation does not match"))
    } else {
        state
            .auth
            .setup_password(&form.username, &form.password, form.setup_token.as_deref())
            .await
    };

    let next = safe_next_path(form.next.as_deref());
    match result {
        Ok(session) => {
            if configured {
                Ok(redirect_with_site_session(
                    next.as_deref().unwrap_or("/change-feed"),
                    &session.access_token,
                    site_session_cookie_is_secure(&headers),
                ))
            } else {
                let feed = state.vaults.activity_feed(&session.user, 50).await?;
                Ok(Html(render_password_page(
                    true,
                    state.auth.password_setup_token_is_required()?,
                    Some(format!("Access token created for {}.", session.user)),
                    Some(session.access_token.clone()),
                    &feed,
                    next.as_deref(),
                ))
                .into_response())
            }
        }
        Err(error) => Ok(Html(render_password_page(
            configured,
            state.auth.password_setup_token_is_required()?,
            Some(error.to_string()),
            None,
            &[],
            next.as_deref(),
        ))
        .into_response()),
    }
}

async fn change_feed_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(token) = site_session_token(&headers) else {
        return Ok(redirect_to_login());
    };
    let auth = match state.auth.verify_bearer_token(&token).await {
        Ok(auth) => auth,
        Err(_) => return Ok(redirect_to_login()),
    };
    let feed = state.vaults.activity_feed(&auth.user, 50).await?;
    Ok(Html(render_change_feed_page(&auth.user, &feed)).into_response())
}

async fn setup_password(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PasswordAuthRequest>,
) -> Result<Json<crate::password_auth::PasswordAuthSession>, ApiError> {
    if !matches!(state.public_auth, PublicAuthConfig::Password) {
        return Err(ApiError(anyhow::anyhow!("password login is not enabled")));
    }
    Ok(Json(
        state
            .auth
            .setup_password(
                &request.username,
                &request.password,
                request.setup_token.as_deref(),
            )
            .await?,
    ))
}

async fn login_password(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PasswordAuthRequest>,
) -> Result<Json<crate::password_auth::PasswordAuthSession>, ApiError> {
    if !matches!(state.public_auth, PublicAuthConfig::Password) {
        return Err(ApiError(anyhow::anyhow!("password login is not enabled")));
    }
    Ok(Json(
        state
            .auth
            .login_password(&request.username, &request.password)
            .await?,
    ))
}

async fn login_oidc(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OidcLoginRequest>,
) -> Result<Json<crate::app_session::AppSession>, ApiError> {
    if !matches!(state.public_auth, PublicAuthConfig::Oidc { .. }) {
        return Err(ApiError(anyhow::anyhow!("OIDC login is not enabled")));
    }
    Ok(Json(state.auth.login_oidc(&request.access_token).await?))
}

async fn refresh_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RefreshSessionRequest>,
) -> Result<Json<crate::app_session::AppSession>, ApiError> {
    Ok(Json(
        state.auth.refresh_session(&request.refresh_token).await?,
    ))
}

async fn auth_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PublicAuthConfigResponse>, ApiError> {
    let response = match &state.public_auth {
        PublicAuthConfig::Password => PublicAuthConfigResponse::Password {
            password_configured: state.auth.password_is_configured().await?,
            setup_token_required: state.auth.password_setup_token_is_required()?,
        },
        PublicAuthConfig::Oidc {
            issuer,
            client_id,
            scope,
            audience,
        } => PublicAuthConfigResponse::Oidc {
            issuer: issuer.clone(),
            client_id: client_id.clone(),
            scope: scope.clone(),
            audience: audience.clone(),
        },
        PublicAuthConfig::Token => PublicAuthConfigResponse::Token,
    };
    Ok(Json(response))
}

fn render_home_page(public_auth: &PublicAuthConfig) -> String {
    let auth_line = match public_auth {
        PublicAuthConfig::Password => "Password login is enabled. Open /login to sign in.",
        PublicAuthConfig::Oidc { issuer, .. } => {
            return format!(
                r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OBSync</title>
<style>
:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
main {{ width: min(720px, calc(100vw - 32px)); padding: 2rem 0; }}
h1 {{ font-size: 1.7rem; margin: 0 0 0.75rem; }}
p {{ line-height: 1.5; color: color-mix(in srgb, CanvasText 76%, transparent); }}
code {{ font: 0.95em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
</style>
</head>
<body>
<main>
<h1>OBSync</h1>
<p>This sync server is running and uses Zitadel OIDC for plugin login.</p>
<p>Issuer: <code>{}</code></p>
<p>Use the Obsidian plugin login button to start device authorization.</p>
</main>
</body>
</html>"#,
                escape_html(issuer)
            );
        }
        PublicAuthConfig::Token => "Static token authentication is enabled.",
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OBSync</title>
<style>
:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
main {{ width: min(720px, calc(100vw - 32px)); padding: 2rem 0; }}
h1 {{ font-size: 1.7rem; margin: 0 0 0.75rem; }}
p {{ line-height: 1.5; color: color-mix(in srgb, CanvasText 76%, transparent); }}
</style>
</head>
<body>
<main>
<h1>OBSync</h1>
<p>{}</p>
</main>
</body>
</html>"#,
        escape_html(auth_line)
    )
}

async fn auth_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AuthSessionResponse>, ApiError> {
    let auth = state.auth.verify_headers(&headers).await?;
    Ok(Json(AuthSessionResponse {
        user: auth.user,
        subject: auth.subject,
    }))
}

fn render_password_page(
    configured: bool,
    setup_token_required: bool,
    message: Option<String>,
    token: Option<String>,
    feed: &[ActivityFeedEntry],
    next: Option<&str>,
) -> String {
    let next_field = next
        .map(|value| {
            format!(
                r#"<input type="hidden" name="next" value="{}">"#,
                escape_html(value)
            )
        })
        .unwrap_or_default();
    let title = if configured { "Log in" } else { "Set password" };
    let password_label = if configured {
        "Password"
    } else {
        "New password"
    };
    let confirm_field = if configured {
        String::new()
    } else {
        r#"<label>Confirm password<input name="password_confirm" type="password" autocomplete="new-password" required></label>"#.to_string()
    };
    let setup_token_field = if configured || !setup_token_required {
        String::new()
    } else {
        r#"<label>Setup token<input name="setup_token" type="password" autocomplete="one-time-code" required></label>"#.to_string()
    };
    let autocomplete = if configured {
        "current-password"
    } else {
        "new-password"
    };
    let message_html = message
        .as_deref()
        .map(|value| format!(r#"<p class="message">{}</p>"#, escape_html(value)))
        .unwrap_or_default();
    let token_html = token
        .as_deref()
        .map(|value| {
            format!(
                "<section><h2>Access token</h2><p>Paste this token into the Obsidian plugin access token field.</p><textarea readonly>{}</textarea></section>",
                escape_html(value)
            )
        })
        .unwrap_or_default();
    let feed_html = render_feed(feed);

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - ObsidiSync</title>
<style>
:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
main {{ width: min(760px, calc(100vw - 32px)); padding: 2rem 0; }}
h1 {{ font-size: 1.5rem; margin: 0 0 1rem; }}
form, section {{ display: grid; gap: 1rem; }}
label {{ display: grid; gap: 0.35rem; font-weight: 600; }}
input, textarea, button {{ font: inherit; border: 1px solid color-mix(in srgb, CanvasText 24%, transparent); border-radius: 6px; padding: 0.7rem; }}
textarea {{ min-height: 8rem; resize: vertical; }}
button {{ cursor: pointer; font-weight: 700; }}
.message {{ margin: 0 0 1rem; color: CanvasText; }}
.feed {{ margin-top: 2rem; }}
.feed ol {{ list-style: none; padding: 0; margin: 0; display: grid; gap: 0.9rem; }}
.feed li {{ border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 8px; padding: 0.9rem; }}
.feed h3 {{ margin: 0 0 0.25rem; font-size: 1rem; }}
.meta, .files {{ margin: 0; color: color-mix(in srgb, CanvasText 72%, transparent); font-size: 0.88rem; }}
.files {{ margin-top: 0.5rem; overflow-wrap: anywhere; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
{message_html}
<form action="/login" method="post">
{next_field}
<label>Username<input name="username" type="text" autocomplete="username" required autofocus></label>
<label>{password_label}<input name="password" type="password" autocomplete="{autocomplete}" required></label>
{confirm_field}
{setup_token_field}
<button type="submit">{title}</button>
</form>
{token_html}
{feed_html}
</main>
</body>
</html>"#
    )
}

fn render_change_feed_page(user: &str, feed: &[ActivityFeedEntry]) -> String {
    let feed_html = render_feed(feed);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Change feed - ObsidiSync</title>
<style>
:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
body {{ margin: 0; min-height: 100vh; background: Canvas; color: CanvasText; }}
main {{ width: min(860px, calc(100vw - 32px)); margin: 0 auto; padding: 2rem 0; }}
h1 {{ font-size: 1.6rem; margin: 0 0 0.25rem; }}
.meta, .files {{ margin: 0; color: color-mix(in srgb, CanvasText 72%, transparent); font-size: 0.88rem; }}
.feed {{ margin-top: 2rem; }}
.feed ol {{ list-style: none; padding: 0; margin: 0; display: grid; gap: 0.9rem; }}
.feed li {{ border: 1px solid color-mix(in srgb, CanvasText 18%, transparent); border-radius: 8px; padding: 0.9rem; }}
.feed h3 {{ margin: 0 0 0.25rem; font-size: 1rem; }}
.files {{ margin-top: 0.5rem; overflow-wrap: anywhere; }}
</style>
</head>
<body>
<main>
<h1>Change feed</h1>
<p class="meta">Signed in as {}</p>
{feed_html}
</main>
</body>
</html>"#,
        escape_html(user)
    )
}

fn render_feed(feed: &[ActivityFeedEntry]) -> String {
    let items = if feed.is_empty() {
        r#"<p class="meta">No synced changes yet.</p>"#.to_string()
    } else {
        let entries = feed
            .iter()
            .map(|entry| {
                let files = if entry.files.is_empty() {
                    "No file list recorded".to_string()
                } else {
                    let shown = entry
                        .files
                        .iter()
                        .take(8)
                        .map(|path| escape_html(path))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if entry.files.len() > 8 {
                        format!("{} and {} more", shown, entry.files.len() - 8)
                    } else {
                        shown
                    }
                };
                format!(
                    r#"<li><h3>{}</h3><p class="meta">{} · {} · {} · {}</p><p class="files">{}</p></li>"#,
                    escape_html(&entry.subject),
                    escape_html(&entry.vault),
                    escape_html(&entry.author),
                    escape_html(&entry.date),
                    escape_html(&entry.hash.chars().take(12).collect::<String>()),
                    files,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!("<ol>{entries}</ol>")
    };
    format!(r#"<section class="feed"><h2>Recent changes</h2>{items}</section>"#)
}

pub(crate) fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.register(&user, &vault, request).await?))
}

async fn feed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user): Path<String>,
) -> Result<Json<Vec<ActivityFeedEntry>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.activity_feed(&user, 50).await?))
}

async fn sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    let changed: Vec<String> = request
        .changes
        .iter()
        .map(|change| match change {
            ClientChange::Upsert { path, .. } | ClientChange::Delete { path } => path.clone(),
        })
        .collect();
    let response = state.vaults.sync(&user, &vault, request).await?;
    state.tablet.schedule(&user, &vault, changed);
    Ok(Json(response))
}

async fn init_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<UploadInitRequest>,
) -> Result<Json<UploadInitResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state.vaults.init_upload(&user, &vault, request).await?,
    ))
}

async fn upload_chunk(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault, upload)): Path<(String, String, String)>,
    Json(request): Json<UploadChunkRequest>,
) -> Result<Json<UploadChunkResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state
            .vaults
            .append_upload_chunk(&user, &vault, &upload, request)
            .await?,
    ))
}

async fn complete_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault, upload)): Path<(String, String, String)>,
) -> Result<Json<UploadCompleteResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state.vaults.complete_upload(&user, &vault, &upload).await?,
    ))
}

async fn history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<HistoryEntry>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state
            .vaults
            .history(&user, &vault, query.path.as_deref())
            .await?,
    ))
}

async fn file_at_version(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Query(query): Query<FileQuery>,
) -> Result<Json<VersionFileResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state
            .vaults
            .file_at_version(&user, &vault, &query.path, &query.hash)
            .await?,
    ))
}

/// Raw file bytes at a commit. Complements `sync` in `FileContentMode::Reference`, where the
/// client downloads each changed file separately instead of receiving the vault as one JSON body.
async fn blob_at_version(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers, &user).await?;
    let (path, content) = state
        .vaults
        .file_bytes_at_version(&user, &vault, &query.path, &query.hash)
        .await?;
    let sha256 = crate::binary_store::sha256_hex(&content);
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                crate::webdav::content_type_for(&path).to_string(),
            ),
            (header::CONTENT_LENGTH, content.len().to_string()),
            (header::ETAG, format!("\"{sha256}\"")),
            (header::CACHE_CONTROL, "private, max-age=0".to_string()),
            (header::HeaderName::from_static("x-content-sha256"), sha256),
        ],
        content,
    )
        .into_response())
}

async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.resolve(&user, &vault, request).await?))
}

async fn devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
) -> Result<Json<Vec<DeviceEntry>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.list_devices(&user, &vault).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingConflictsQuery {
    client_id: String,
}

async fn pending_conflicts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Query(query): Query<PendingConflictsQuery>,
) -> Result<Json<Vec<SyncConflict>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state
            .vaults
            .pending_conflicts_for(&user, &vault, &query.client_id)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct DeviceVersionsQuery {
    path: String,
}

async fn device_versions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Query(query): Query<DeviceVersionsQuery>,
) -> Result<Json<Vec<DeviceVersionEntry>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(
        state
            .vaults
            .device_versions(&user, &vault, &query.path)
            .await?,
    ))
}

async fn set_version_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<VersionMetadataRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers, &user).await?;
    state
        .vaults
        .set_version_metadata(&user, &vault, request)
        .await?;
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
    hash: String,
}

async fn list_device_passwords(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
) -> Result<Json<Vec<DevicePasswordEntry>>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.device_passwords.list(&user, &vault).await?))
}

async fn create_device_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<CreateDevicePasswordRequest>,
) -> Result<Json<CreatedDevicePassword>, ApiError> {
    authorize(&state, &headers, &user).await?;
    if !state.vaults.is_registered(&user, &vault).await {
        return Err(ApiError(anyhow::anyhow!(
            "invalid vault: sync this vault from Obsidian once before creating device passwords"
        )));
    }
    Ok(Json(
        state
            .device_passwords
            .create(&user, &vault, request)
            .await?,
    ))
}

async fn revoke_device_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault, id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers, &user).await?;
    if !state.device_passwords.revoke(&user, &vault, &id).await? {
        return Err(ApiError(anyhow::anyhow!("not found: device password")));
    }
    Ok(Json(serde_json::json!({ "revoked": true })))
}

async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    requested_user: &str,
) -> Result<(), ApiError> {
    let auth = state.auth.verify_headers(headers).await?;
    tracing::debug!(subject = %auth.subject, user = %auth.user, requested_user = %requested_user, "authorized request");
    if auth.user != requested_user {
        return Err(ApiError(anyhow::anyhow!(
            "forbidden: token user {} cannot access {}",
            auth.user,
            requested_user
        )));
    }
    Ok(())
}

pub(crate) fn redirect_with_site_session(
    location: &str,
    access_token: &str,
    secure: bool,
) -> Response {
    let secure_attribute = if secure { "; Secure" } else { "" };
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, location.to_string()),
            (
                header::SET_COOKIE,
                // Lax, not Strict: after an OIDC login the browser arrives here through a
                // cross-site redirect from the issuer, and Strict cookies are withheld on the
                // navigations that follow it, which would loop the login forever.
                format!(
                    "{SITE_SESSION_COOKIE}={}; Path=/; Max-Age={SITE_SESSION_COOKIE_MAX_AGE_SECONDS}; HttpOnly; SameSite=Lax{secure_attribute}",
                    cookie_encode(access_token),
                ),
            ),
        ],
    )
        .into_response()
}

pub(crate) fn site_session_cookie_is_secure(headers: &HeaderMap) -> bool {
    header_contains_token(headers, "x-forwarded-proto", "https")
        || header_contains_token(headers, "x-forwarded-ssl", "on")
        || headers
            .get("forwarded")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split([',', ';'])
                    .any(|part| part.trim().eq_ignore_ascii_case("proto=https"))
            })
}

fn header_contains_token(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(expected))
        })
}

fn redirect_to_login() -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response()
}

pub(crate) fn site_session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == SITE_SESSION_COOKIE).then(|| cookie_decode(value).unwrap_or_default())
        })
        .filter(|value| !value.is_empty())
}

fn cookie_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn cookie_decode(value: &str) -> Option<String> {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            output.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}
