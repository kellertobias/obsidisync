//! Just enough of Nextcloud's HTTP surface for the Saber app to log in and sync.
//!
//! Saber (https://github.com/saber-notes/saber) treats any URL as a Nextcloud server and uses:
//!
//! - Login Flow v2: `POST /index.php/login/v2` returns a browser login URL and a poll token;
//!   the app polls `POST /index.php/login/v2/poll` until the browser has finished, and receives
//!   an app password. Here the browser page asks the user which vault and folder to sync to and
//!   for the Saber encryption password, then issues a device password.
//! - `GET /ocs/v2.php/cloud/user` (OCS provisioning API) to learn the account id.
//! - `GET /index.php/avatar/{user}/{size}` for the profile picture.
//! - WebDAV under `/remote.php/webdav/` (and `/remote.php/dav/files/{user}/`), served by
//!   `crate::webdav` with the granted folder exposed as `Saber/`.
//!
//! Everything else Nextcloud offers is absent; unknown paths fall through to 404.

use crate::device_passwords::CreateSaberDevice;
use crate::http::{escape_html, site_session_token, AppState, PublicAuthConfig};
use crate::webdav::{authenticate_device, client_ip_for, handle_mounted, Mount};
use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, Path as AxumPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long the app has to complete the browser part of the login.
const LOGIN_FLOW_TTL: Duration = Duration::from_secs(20 * 60);
const MAX_PENDING_FLOWS: usize = 200;
/// Default vault folder for Saber's encrypted files: a dot-folder below the PDF folder, so
/// Obsidian's file explorer hides the encrypted blobs while the PDFs sit next to them.
pub const DEFAULT_SYNC_FOLDER: &str = "Tablet/.sync";
/// Default vault folder for the rendered PDFs.
pub const DEFAULT_PDF_FOLDER: &str = "Tablet";
/// Nextcloud version reported by `status.php`; Saber does not check it, but the client library
/// wants something parseable.
const FAKE_NEXTCLOUD_VERSION: &str = "29.0.0.0";

/// In-memory Login Flow v2 state. Flows are short-lived and only meaningful to the process
/// that created them.
#[derive(Debug, Default)]
pub struct LoginFlowStore {
    flows: Mutex<HashMap<String, LoginFlow>>,
}

#[derive(Debug)]
struct LoginFlow {
    poll_token: String,
    created_at: Instant,
    credentials: Option<FlowCredentials>,
}

#[derive(Debug, Clone)]
struct FlowCredentials {
    login_name: String,
    app_password: String,
}

impl LoginFlowStore {
    async fn create(&self) -> Result<(String, String)> {
        let flow_token = random_token()?;
        let poll_token = random_token()?;
        let mut flows = self.flows.lock().await;
        let now = Instant::now();
        flows.retain(|_, flow| now.duration_since(flow.created_at) < LOGIN_FLOW_TTL);
        if flows.len() >= MAX_PENDING_FLOWS {
            return Err(anyhow!("too many pending login flows"));
        }
        flows.insert(
            flow_token.clone(),
            LoginFlow {
                poll_token: poll_token.clone(),
                created_at: now,
                credentials: None,
            },
        );
        Ok((flow_token, poll_token))
    }

    async fn status(&self, flow_token: &str) -> FlowStatus {
        let flows = self.flows.lock().await;
        match flows.get(flow_token) {
            Some(flow) if flow.is_expired() => FlowStatus::Missing,
            Some(flow) if flow.credentials.is_some() => FlowStatus::Completed,
            Some(_) => FlowStatus::Pending,
            None => FlowStatus::Missing,
        }
    }

    async fn is_pending(&self, flow_token: &str) -> bool {
        self.status(flow_token).await == FlowStatus::Pending
    }

    async fn complete(&self, flow_token: &str, credentials: FlowCredentials) -> bool {
        let mut flows = self.flows.lock().await;
        match flows.get_mut(flow_token) {
            Some(flow) if flow.credentials.is_none() && !flow.is_expired() => {
                flow.credentials = Some(credentials);
                true
            }
            _ => false,
        }
    }

    /// Returns the credentials once and forgets the flow.
    async fn poll(&self, poll_token: &str) -> Option<FlowCredentials> {
        let mut flows = self.flows.lock().await;
        let key = flows
            .iter()
            .find(|(_, flow)| flow.poll_token == poll_token)
            .map(|(key, _)| key.clone())?;
        let flow = flows.get(&key)?;
        if flow.is_expired() {
            flows.remove(&key);
            return None;
        }
        flow.credentials.as_ref()?;
        flows.remove(&key).and_then(|flow| flow.credentials)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowStatus {
    Pending,
    Completed,
    Missing,
}

impl LoginFlow {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= LOGIN_FLOW_TTL
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/status.php", get(status))
        .route("/index.php/login/v2", post(login_flow_init))
        .route("/index.php/login/v2/poll", post(login_flow_poll))
        .route(
            "/index.php/login/v2/flow/:token",
            get(login_flow_page).post(login_flow_submit),
        )
        .route("/ocs/v1.php/cloud/user", get(ocs_user))
        .route("/ocs/v2.php/cloud/user", get(ocs_user))
        .route("/ocs/v1.php/cloud/capabilities", get(ocs_capabilities))
        .route("/ocs/v2.php/cloud/capabilities", get(ocs_capabilities))
        .route("/index.php/avatar/:user/:size", get(avatar))
        .route("/avatar/:user/:size", get(avatar))
        .route("/remote.php/webdav", any(webdav))
        .route("/remote.php/webdav/", any(webdav))
        .route("/remote.php/webdav/*path", any(webdav))
        .route("/remote.php/dav", any(webdav))
        .route("/remote.php/dav/", any(webdav))
        .route("/remote.php/dav/*path", any(webdav))
}

async fn status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "installed": true,
        "maintenance": false,
        "needsDbUpgrade": false,
        "version": FAKE_NEXTCLOUD_VERSION,
        "versionstring": FAKE_NEXTCLOUD_VERSION.trim_end_matches(".0"),
        "edition": "",
        "productname": "ObsidiSync",
        "extendedSupport": false
    }))
}

async fn login_flow_init(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let base = public_base_url(&headers);
    match state.login_flows.create().await {
        Ok((flow_token, poll_token)) => Json(serde_json::json!({
            "poll": {
                "token": poll_token,
                "endpoint": format!("{base}/index.php/login/v2/poll"),
            },
            "login": format!("{base}/index.php/login/v2/flow/{flow_token}"),
        }))
        .into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct PollBody {
    token: String,
}

async fn login_flow_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let token = if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
    {
        serde_json::from_str::<PollBody>(&body)
            .ok()
            .map(|body| body.token)
    } else {
        serde_urlencoded::from_str::<PollBody>(&body)
            .ok()
            .map(|body| body.token)
            .or_else(|| {
                serde_json::from_str::<PollBody>(&body)
                    .ok()
                    .map(|body| body.token)
            })
    };
    let Some(token) = token else {
        return (StatusCode::BAD_REQUEST, "token is required").into_response();
    };
    match state.login_flows.poll(&token).await {
        Some(credentials) => Json(serde_json::json!({
            "server": public_base_url(&headers),
            "loginName": credentials.login_name,
            "appPassword": credentials.app_password,
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct FlowQuery {
    #[serde(default)]
    error: Option<String>,
}

async fn login_flow_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Query(query): Query<FlowQuery>,
) -> Response {
    match state.login_flows.status(&token).await {
        FlowStatus::Pending => {}
        FlowStatus::Completed => {
            return Html(render_message_page(
                "Saber is already connected",
                "This login was completed. Return to Saber; it picks up the login by itself. If Saber still shows the login screen, start the login again.",
            ))
            .into_response();
        }
        FlowStatus::Missing => {
            return (
                StatusCode::NOT_FOUND,
                Html(render_message_page(
                    "Login link expired",
                    "This Saber login link is no longer valid (links last 20 minutes). Start the login again in Saber.",
                )),
            )
                .into_response();
        }
    }
    let session_user = session_user(&state, &headers).await;
    if session_user.is_none() {
        let next = format!("/index.php/login/v2/flow/{token}");
        match &state.public_auth {
            PublicAuthConfig::Password => {
                return Redirect::to(&format!("/login?next={}", urlencode(&next))).into_response();
            }
            PublicAuthConfig::Oidc { .. } => {
                return Redirect::to(&crate::oidc_login::start_url(&next)).into_response();
            }
            PublicAuthConfig::Token => {}
        }
    }
    let (vaults, default_vault) = match &session_user {
        Some(user) => (
            state.vaults.list_vaults(user).await.unwrap_or_default(),
            state.vaults.default_vault(user).await.ok().flatten(),
        ),
        None => (Vec::new(), None),
    };
    if session_user.is_some() && vaults.is_empty() {
        return Html(render_message_page(
            "No vault yet",
            "Sync a vault from Obsidian with this account first, then start the Saber login again.",
        ))
        .into_response();
    }
    Html(render_flow_page(
        &token,
        session_user.as_deref(),
        &vaults,
        default_vault.as_deref(),
        &state.public_auth,
        query.error.as_deref(),
    ))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct FlowForm {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    vault: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    pdf_folder: String,
    #[serde(default)]
    encryption_password: String,
}

async fn login_flow_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Form(form): Form<FlowForm>,
) -> Response {
    if !state.login_flows.is_pending(&token).await {
        return (
            StatusCode::NOT_FOUND,
            Html(render_message_page(
                "Login link expired",
                "This Saber login link is no longer valid. Start the login again in Saber.",
            )),
        )
            .into_response();
    }
    let user = match session_user(&state, &headers).await {
        Some(user) => Some(user),
        None if !form.access_token.trim().is_empty() => state
            .auth
            .verify_bearer_token(form.access_token.trim())
            .await
            .ok()
            .map(|auth| auth.user),
        None => None,
    };
    let Some(user) = user else {
        return flow_error(
            &token,
            "Not signed in. Log in first, or paste a valid access token.",
        );
    };

    // The user's default vault unless the form explicitly named another registered one.
    let vault = match form.vault.trim() {
        "" => state
            .vaults
            .default_vault(&user)
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        named => named.to_string(),
    };
    if vault.is_empty() || !state.vaults.is_registered(&user, &vault).await {
        return flow_error(
            &token,
            "Unknown vault. Sync the vault from Obsidian once before connecting Saber.",
        );
    }
    let label = if form.label.trim().is_empty() {
        "Saber".to_string()
    } else {
        form.label.trim().to_string()
    };
    let folder = if form.folder.trim().is_empty() {
        DEFAULT_SYNC_FOLDER.to_string()
    } else {
        form.folder.clone()
    };
    let pdf_folder = if form.pdf_folder.trim().is_empty() {
        DEFAULT_PDF_FOLDER.to_string()
    } else {
        form.pdf_folder.clone()
    };
    let created = match state
        .device_passwords
        .create_saber(
            &user,
            &vault,
            CreateSaberDevice {
                label,
                folder,
                pdf_folder,
                encryption_password: form.encryption_password,
            },
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return flow_error(&token, &error.to_string()),
    };

    let credentials = FlowCredentials {
        login_name: user.clone(),
        app_password: created.password.clone(),
    };
    if !state.login_flows.complete(&token, credentials).await {
        return flow_error(&token, "This login link was already used.");
    }
    let base = public_base_url(&headers);
    Html(render_done_page(&base, &user, &created.password)).into_response()
}

fn flow_error(token: &str, message: &str) -> Response {
    Redirect::to(&format!(
        "/index.php/login/v2/flow/{token}?error={}",
        urlencode(message)
    ))
    .into_response()
}

async fn session_user(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let token = site_session_token(headers)?;
    state
        .auth
        .verify_bearer_token(&token)
        .await
        .ok()
        .map(|auth| auth.user)
}

async fn ocs_user(
    State(state): State<Arc<AppState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Response {
    let client_ip = client_ip_for(&headers, connect_info);
    let grant = match authenticate_device(&state, &headers, &client_ip).await {
        Ok(grant) => grant,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "id": grant.user,
                "displayname": grant.user,
                "display-name": grant.user,
                "email": serde_json::Value::Null,
                "enabled": true,
                "quota": { "free": 0, "used": 0, "total": 0, "relative": 0, "quota": -3 },
                "language": "en",
                "locale": "en",
                "storageLocation": "",
                "lastLogin": 0,
                "backend": "ObsidiSync",
                "subadmin": [],
                "groups": [],
                "phone": "",
                "address": "",
                "website": "",
                "twitter": "",
                "fediverse": "",
                "organisation": "",
                "role": "",
                "headline": "",
                "biography": "",
                "profile_enabled": "0",
                "pronouns": "",
                "additional_mail": [],
                "backendCapabilities": { "setDisplayName": false, "setPassword": false },
                "notify_email": serde_json::Value::Null,
                "manager": ""
            }
        }
    }))
    .into_response()
}

async fn ocs_capabilities() -> Json<serde_json::Value> {
    let (major, minor, micro) = version_parts();
    Json(serde_json::json!({
        "ocs": {
            "meta": { "status": "ok", "statuscode": 200, "message": "OK" },
            "data": {
                "version": {
                    "major": major, "minor": minor, "micro": micro,
                    "string": FAKE_NEXTCLOUD_VERSION.trim_end_matches(".0"),
                    "edition": "", "extendedSupport": false
                },
                "capabilities": {
                    "core": { "pollinterval": 60, "webdav-root": "remote.php/webdav" },
                    "files": { "bigfilechunking": false, "versioning": false }
                }
            }
        }
    }))
}

fn version_parts() -> (u32, u32, u32) {
    let mut parts = FAKE_NEXTCLOUD_VERSION
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    (
        parts.next().unwrap_or(29),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// A plain single-colour PNG. Saber fetches the avatar right after login and only logs
/// failures, but a real image keeps its profile widget tidy.
async fn avatar(AxumPath((_user, size)): AxumPath<(String, String)>) -> Response {
    let size = size.parse::<u32>().unwrap_or(64).clamp(16, 512);
    let mut buffer = std::io::Cursor::new(Vec::new());
    let image = image::RgbImage::from_pixel(size, size, image::Rgb([0x4a, 0x6c, 0xf7]));
    if image::DynamicImage::ImageRgb8(image)
        .write_to(&mut buffer, image::ImageFormat::Png)
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        buffer.into_inner(),
    )
        .into_response()
}

async fn webdav(
    State(state): State<Arc<AppState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    request: Request<Body>,
) -> Response {
    let path = request.uri().path().to_string();
    let prefix = match mount_prefix(&path) {
        Some(prefix) => prefix,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    handle_mounted(&state, connect_info, request, &Mount::nextcloud(&prefix)).await
}

/// Which Nextcloud WebDAV root a path belongs to: `/remote.php/webdav` or
/// `/remote.php/dav/files/{user}`.
fn mount_prefix(path: &str) -> Option<String> {
    if path == "/remote.php/webdav" || path.starts_with("/remote.php/webdav/") {
        return Some("/remote.php/webdav".to_string());
    }
    let rest = path.strip_prefix("/remote.php/dav/files/")?;
    let user = rest.split('/').next()?;
    if user.is_empty() {
        return None;
    }
    Some(format!("/remote.php/dav/files/{user}"))
}

/// The URL Saber should use to reach this server, reconstructed from proxy headers.
pub fn public_base_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| *value == "http" || *value == "https")
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && !value.contains('/') && !value.contains(char::is_whitespace)
        })
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("random generator failed: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn urlencode(value: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

const PAGE_STYLE: &str = r#"
:root { color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }
main { width: min(680px, calc(100vw - 32px)); padding: 2rem 0; }
h1 { font-size: 1.5rem; margin: 0 0 0.5rem; }
p, li { line-height: 1.5; }
.muted { color: color-mix(in srgb, CanvasText 72%, transparent); font-size: 0.92rem; }
form { display: grid; gap: 1rem; margin-top: 1rem; }
label { display: grid; gap: 0.35rem; font-weight: 600; }
label span { font-weight: 400; }
input, select, button { font: inherit; border: 1px solid color-mix(in srgb, CanvasText 24%, transparent); border-radius: 6px; padding: 0.7rem; }
button { cursor: pointer; font-weight: 700; }
.error { border: 1px solid #c0392b; border-radius: 6px; padding: 0.7rem; color: #c0392b; }
code { font: 0.95em ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
.password { font: 1.3rem ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; letter-spacing: 0.05em; padding: 0.7rem; border: 1px solid color-mix(in srgb, CanvasText 24%, transparent); border-radius: 6px; user-select: all; }
a.button { display: inline-block; padding: 0.8rem 1.2rem; border-radius: 6px; background: #4a6cf7; color: white; text-decoration: none; font-weight: 700; }
"#;

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - ObsidiSync</title>
<style>{PAGE_STYLE}</style>
</head>
<body>
<main>
{body}
</main>
</body>
</html>"#,
        title = escape_html(title),
    )
}

fn render_message_page(title: &str, message: &str) -> String {
    page(
        title,
        &format!(
            "<h1>{}</h1><p>{}</p>",
            escape_html(title),
            escape_html(message)
        ),
    )
}

fn render_flow_page(
    token: &str,
    user: Option<&str>,
    vaults: &[String],
    default_vault: Option<&str>,
    public_auth: &PublicAuthConfig,
    error: Option<&str>,
) -> String {
    let error_html = error
        .map(|message| format!(r#"<p class="error">{}</p>"#, escape_html(message)))
        .unwrap_or_default();
    let identity_html = match user {
        Some(user) => format!(
            r#"<p class="muted">Signed in as <strong>{}</strong>.</p>"#,
            escape_html(user)
        ),
        None => {
            let hint = match public_auth {
                PublicAuthConfig::Oidc { .. } => "This server uses OIDC. Paste the access token from the Obsidian plugin's advanced settings.",
                PublicAuthConfig::Token => "Paste the server's access token.",
                PublicAuthConfig::Password => "Paste an access token.",
            };
            format!(
                r#"<label>Access token<span class="muted">{}</span><input name="access_token" type="password" autocomplete="off" required></label>"#,
                escape_html(hint)
            )
        }
    };
    let default_vault = default_vault.or(vaults.first().map(String::as_str));
    let vault_html = match (vaults.len(), default_vault) {
        (0, _) | (_, None) => {
            r#"<label>Vault<span class="muted">The vault name used in the Obsidian plugin.</span><input name="vault" type="text" required></label>"#.to_string()
        }
        (1, Some(vault)) => format!(
            r#"<p class="muted">Vault: <strong>{0}</strong></p><input type="hidden" name="vault" value="{0}">"#,
            escape_html(vault)
        ),
        (_, Some(default)) => {
            let options = vaults
                .iter()
                .map(|vault| {
                    format!(
                        r#"<option value="{0}"{1}>{0}</option>"#,
                        escape_html(vault),
                        if vault == default { " selected" } else { "" }
                    )
                })
                .collect::<String>();
            format!(r#"<label>Vault<span class="muted">Your most recently synced vault is preselected.</span><select name="vault">{options}</select></label>"#)
        }
    };
    let body = format!(
        r#"<h1>Connect Saber</h1>
<p>Saber is asking to sync its notes to this ObsidiSync server. Choose where they go in your vault.</p>
{error_html}
<form method="post" action="/index.php/login/v2/flow/{token}">
{identity_html}
{vault_html}
<label>Device name<span class="muted">Shown in file history, for example "Saber on iPad".</span><input name="label" type="text" value="Saber" maxlength="80"></label>
<label>Sync folder<span class="muted">Receives Saber's encrypted files. Keep it separate from your notes.</span><input name="folder" type="text" value="{sync_folder}"></label>
<label>PDF folder<span class="muted">Rendered PDFs are written here, mirroring Saber's folders.</span><input name="pdf_folder" type="text" value="{pdf_folder}"></label>
<label>Saber encryption password<span class="muted">The encryption password you will enter in Saber after this step. The server stores it to decrypt your notes and render PDFs. Leave empty to only store the encrypted files.</span><input name="encryption_password" type="password" autocomplete="off"></label>
<button type="submit">Connect</button>
</form>"#,
        token = escape_html(token),
        sync_folder = escape_html(DEFAULT_SYNC_FOLDER),
        pdf_folder = escape_html(DEFAULT_PDF_FOLDER),
    );
    page("Connect Saber", &body)
}

fn render_done_page(base: &str, user: &str, password: &str) -> String {
    // The scheme Nextcloud's own flow uses to hand credentials back to a native app.
    let callback = format!(
        "nc://login/server:{base}&user:{}&password:{}",
        urlencode(user),
        urlencode(password)
    );
    let body = format!(
        r#"<h1>Saber is connected</h1>
<p>Return to Saber; it picks up the login automatically. If it does not, use the button below or enter the password by hand.</p>
<p><a class="button" href="{callback}">Open Saber</a></p>
<p class="muted">Username <code>{user}</code>, password:</p>
<p class="password">{password}</p>
<p class="muted">Next, Saber asks for the encryption password. Enter the same one you gave here. The device can be revoked at any time from Settings → ObsidiSync → Device passwords in Obsidian.</p>
<script>setTimeout(function () {{ window.location.href = {callback_json}; }}, 800);</script>"#,
        callback = escape_html(&callback),
        callback_json = serde_json::to_string(&callback).unwrap_or_else(|_| "\"\"".to_string()),
        user = escape_html(user),
        password = escape_html(password),
    );
    page("Saber is connected", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_base_url_from_proxy_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "sync.example.com".parse().unwrap());
        assert_eq!(public_base_url(&headers), "http://sync.example.com");
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(public_base_url(&headers), "https://sync.example.com");
        headers.insert("x-forwarded-host", "public.example.com".parse().unwrap());
        assert_eq!(public_base_url(&headers), "https://public.example.com");
    }

    #[test]
    fn maps_nextcloud_webdav_prefixes() {
        assert_eq!(
            mount_prefix("/remote.php/webdav/Saber/x.sbe").as_deref(),
            Some("/remote.php/webdav")
        );
        assert_eq!(
            mount_prefix("/remote.php/webdav").as_deref(),
            Some("/remote.php/webdav")
        );
        assert_eq!(
            mount_prefix("/remote.php/dav/files/alice/Saber/").as_deref(),
            Some("/remote.php/dav/files/alice")
        );
        assert_eq!(mount_prefix("/remote.php/dav/"), None);
        assert_eq!(mount_prefix("/remote.php/dav/files/"), None);
    }

    #[tokio::test]
    async fn login_flow_completes_once() {
        let store = LoginFlowStore::default();
        let (flow, poll) = store.create().await.unwrap();
        assert!(store.is_pending(&flow).await);
        assert!(store.poll(&poll).await.is_none());
        assert!(
            store
                .complete(
                    &flow,
                    FlowCredentials {
                        login_name: "alice".into(),
                        app_password: "pw".into()
                    }
                )
                .await
        );
        assert!(!store.is_pending(&flow).await);
        let credentials = store.poll(&poll).await.unwrap();
        assert_eq!(credentials.login_name, "alice");
        assert!(store.poll(&poll).await.is_none());
    }
}
