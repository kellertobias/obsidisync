use crate::auth::AuthVerifier;
use crate::protocol::*;
use crate::vault::VaultService;
use axum::extract::{DefaultBodyLimit, Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
pub struct AppState {
    pub vaults: VaultService,
    pub auth: AuthVerifier,
}

#[derive(Debug)]
pub struct ApiError(anyhow::Error);

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub path: Option<String>,
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
    let router = Router::new()
        .route("/", get(password_page))
        .route("/login", get(password_page).post(password_form))
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "ok": true })) }),
        )
        .route("/v1/auth/password/setup", post(setup_password))
        .route("/v1/auth/password/login", post(login_password))
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
        .route("/v1/users/:user/vaults/:vault/resolve", post(resolve))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(Arc::new(state));

    apply_cors(router, allowed_origins)
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
        || message.starts_with("password is already set")
        || message.starts_with("password confirmation")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordAuthRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct PasswordLoginForm {
    username: String,
    password: String,
    password_confirm: Option<String>,
}

async fn password_page(State(state): State<Arc<AppState>>) -> Result<Html<String>, ApiError> {
    let configured = state.auth.password_is_configured().await?;
    Ok(Html(render_password_page(configured, None, None)))
}

async fn password_form(
    State(state): State<Arc<AppState>>,
    Form(form): Form<PasswordLoginForm>,
) -> Result<Html<String>, ApiError> {
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
            .setup_password(&form.username, &form.password)
            .await
    };

    match result {
        Ok(session) => Ok(Html(render_password_page(
            true,
            Some(format!("Access token created for {}.", session.user)),
            Some(session.access_token),
        ))),
        Err(error) => Ok(Html(render_password_page(
            configured,
            Some(error.to_string()),
            None,
        ))),
    }
}

async fn setup_password(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PasswordAuthRequest>,
) -> Result<Json<crate::password_auth::PasswordAuthSession>, ApiError> {
    Ok(Json(
        state
            .auth
            .setup_password(&request.username, &request.password)
            .await?,
    ))
}

async fn login_password(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PasswordAuthRequest>,
) -> Result<Json<crate::password_auth::PasswordAuthSession>, ApiError> {
    Ok(Json(
        state
            .auth
            .login_password(&request.username, &request.password)
            .await?,
    ))
}

fn render_password_page(
    configured: bool,
    message: Option<String>,
    token: Option<String>,
) -> String {
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

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - Obsync</title>
<style>
:root {{ color-scheme: light dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
main {{ width: min(440px, calc(100vw - 32px)); }}
h1 {{ font-size: 1.5rem; margin: 0 0 1rem; }}
form, section {{ display: grid; gap: 1rem; }}
label {{ display: grid; gap: 0.35rem; font-weight: 600; }}
input, textarea, button {{ font: inherit; border: 1px solid color-mix(in srgb, CanvasText 24%, transparent); border-radius: 6px; padding: 0.7rem; }}
textarea {{ min-height: 8rem; resize: vertical; }}
button {{ cursor: pointer; font-weight: 700; }}
.message {{ margin: 0 0 1rem; color: CanvasText; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
{message_html}
<form action="/login" method="post">
<label>Username<input name="username" type="text" autocomplete="username" required autofocus></label>
<label>{password_label}<input name="password" type="password" autocomplete="{autocomplete}" required></label>
{confirm_field}
<button type="submit">{title}</button>
</form>
{token_html}
</main>
</body>
</html>"#
    )
}

fn escape_html(value: &str) -> String {
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

async fn sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.sync(&user, &vault, request).await?))
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

async fn resolve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user, vault)): Path<(String, String)>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    authorize(&state, &headers, &user).await?;
    Ok(Json(state.vaults.resolve(&user, &vault, request).await?))
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
    hash: String,
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
