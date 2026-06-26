use crate::auth::AuthVerifier;
use crate::protocol::*;
use crate::vault::VaultService;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
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
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({ "ok": true })) }),
        )
        .route("/v1/users/:user/vaults/:vault/register", post(register))
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
