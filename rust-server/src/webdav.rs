//! Minimal WebDAV (class 1 + advisory class 2 locking) endpoint for external devices.
//!
//! Every request authenticates with HTTP Basic auth using the user's namespace as username and a
//! device password (see `device_passwords`). The device password pins the request to one vault
//! and one folder: `/dav/{vault}/{folder}/...`. Ancestors of that folder are browsable as empty
//! virtual collections so clients that navigate from the root still find their way; everything
//! else is forbidden.

use crate::auth_throttle::AuthThrottle;
use crate::device_passwords::{encode_path_segment, DeviceGrant};
use crate::http::AppState;
use crate::time_format::{
    http_date_from_unix, rfc3339_from_unix, unix_now_millis, unix_seconds_from_millis,
};
use crate::vault::dav::{DavDevice, DavEntry};
use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http_body_util::BodyExt;
use percent_encoding::percent_decode_str;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const DAV_PREFIX: &str = "/dav";
const ALLOWED_METHODS: &str =
    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, COPY, LOCK, UNLOCK";
const LOCK_TIMEOUT_SECONDS: u64 = 3600;
/// Bodies of non-upload methods (PROPFIND, MKCOL, LOCK) are tiny XML documents.
const SMALL_BODY_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
struct DavError {
    status: StatusCode,
    message: String,
    headers: Vec<(HeaderName, String)>,
}

impl DavError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            headers: Vec::new(),
        }
    }

    fn too_many_requests(retry_after_seconds: u64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed login attempts; try again later",
        );
        error
            .headers
            .push((header::RETRY_AFTER, retry_after_seconds.to_string()));
        error
    }

    fn payload_too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("upload exceeds the server limit of {limit} bytes"),
        )
    }

    fn range_not_satisfiable(total: u64) -> Self {
        let mut error = Self::new(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable");
        error
            .headers
            .push((header::CONTENT_RANGE, format!("bytes */{total}")));
        error
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not found")
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl From<anyhow::Error> for DavError {
    fn from(error: anyhow::Error) -> Self {
        let message = error.to_string();
        let status = if message.contains("unauthorized") {
            StatusCode::UNAUTHORIZED
        } else if message.contains("forbidden") {
            StatusCode::FORBIDDEN
        } else if message.contains("not found") {
            StatusCode::NOT_FOUND
        } else if message.contains("conflict") {
            StatusCode::CONFLICT
        } else if message.starts_with("exists") {
            StatusCode::METHOD_NOT_ALLOWED
        } else if message.starts_with("precondition failed") {
            StatusCode::PRECONDITION_FAILED
        } else if message.starts_with("unsafe vault path") || message.starts_with("invalid ") {
            StatusCode::BAD_REQUEST
        } else {
            tracing::warn!(error = %message, "webdav request failed");
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self {
            status,
            message,
            headers: Vec::new(),
        }
    }
}

impl IntoResponse for DavError {
    fn into_response(self) -> Response {
        tracing::debug!(status = %self.status, error = %self.message, "webdav error");
        let body = if self.status.is_server_error() {
            "request failed".to_string()
        } else {
            self.message
        };
        let mut response = (self.status, body).into_response();
        for (name, value) in self.headers {
            if let Ok(value) = HeaderValue::from_str(&value) {
                response.headers_mut().insert(name, value);
            }
        }
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(r#"Basic realm="ObsidiSync WebDAV", charset="UTF-8""#),
            );
        }
        response
    }
}

/// Where a request path lands relative to the device's granted folder.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// An ancestor of the granted folder (`/dav`, `/dav/{vault}`, ...). Browsable, read-only,
    /// and it only ever exposes the one child leading towards the granted folder.
    Ancestor {
        href: String,
        name: String,
        child_name: String,
        child_href: String,
    },
    /// Inside the granted folder. `path` is vault-relative.
    Inside { path: String },
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let client_ip = client_ip(&parts.headers, connect_info.map(|info| info.0));
    match handle_inner(
        &state,
        &parts.uri,
        &parts.method,
        &parts.headers,
        &client_ip,
        body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn handle_inner(
    state: &AppState,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    client_ip: &str,
    body: Body,
) -> Result<Response, DavError> {
    if method == Method::OPTIONS {
        return Ok(options_response());
    }

    let (username, password) = basic_credentials(headers).ok_or_else(DavError::unauthorized)?;
    let throttle_keys = [
        AuthThrottle::ip_key(client_ip),
        AuthThrottle::user_key(&username),
    ];
    if let Some(retry_after) = state.webdav_throttle.blocked_for(&throttle_keys).await {
        return Err(DavError::too_many_requests(retry_after));
    }
    let grant = match state
        .device_passwords
        .authenticate(&username, &password)
        .await
    {
        Ok(grant) => {
            state.webdav_throttle.record_success(&throttle_keys).await;
            grant
        }
        Err(_) => {
            state.webdav_throttle.record_failure(&throttle_keys).await;
            return Err(DavError::unauthorized());
        }
    };

    let segments = decode_path_segments(uri.path())?;
    let target = resolve_target(&grant, &segments)?;

    match method.as_str() {
        "PROPFIND" => propfind(state, &grant, &target, headers).await,
        "GET" | "HEAD" => get(state, &grant, &target, headers, method == Method::HEAD).await,
        "PUT" => put(state, &grant, &target, headers, body).await,
        "DELETE" => delete(state, &grant, &target).await,
        "MKCOL" => {
            let body = read_body_limited(body, SMALL_BODY_LIMIT).await?;
            mkcol(state, &grant, &target, &body).await
        }
        "MOVE" | "COPY" => {
            move_or_copy(state, &grant, &target, headers, method.as_str() == "MOVE").await
        }
        "LOCK" => lock(&grant, &target, headers),
        "UNLOCK" => Ok(StatusCode::NO_CONTENT.into_response()),
        "PROPPATCH" => proppatch(&grant, &target),
        _ => Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, ALLOWED_METHODS)],
        )
            .into_response()),
    }
}

/// Prefers the first `X-Forwarded-For` hop (the server is normally behind a reverse proxy),
/// then the TCP peer. Throttling also keys on the username, so a spoofed header does not
/// disable it.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| peer.map(|address| address.ip().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

async fn read_body_limited(mut body: Body, limit: usize) -> Result<Bytes, DavError> {
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|error| DavError::bad_request(format!("invalid body: {error}")))?;
        if let Ok(data) = frame.into_data() {
            if collected.len() + data.len() > limit {
                return Err(DavError::payload_too_large(limit));
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(collected))
}

/// Streams a request body to disk, enforcing the size limit as bytes arrive.
async fn stream_body_to_file(mut body: Body, path: &Path, limit: usize) -> Result<u64, DavError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(DavError::internal)?;
    let mut written: usize = 0;
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|error| DavError::bad_request(format!("invalid body: {error}")))?;
        if let Ok(data) = frame.into_data() {
            written = written.saturating_add(data.len());
            if written > limit {
                return Err(DavError::payload_too_large(limit));
            }
            file.write_all(&data).await.map_err(DavError::internal)?;
        }
    }
    file.flush().await.map_err(DavError::internal)?;
    Ok(written as u64)
}

fn options_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::ALLOW, ALLOWED_METHODS),
            (header::HeaderName::from_static("dav"), "1, 2"),
            (header::HeaderName::from_static("ms-author-via"), "DAV"),
        ],
    )
        .into_response()
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

fn decode_path_segments(raw_path: &str) -> Result<Vec<String>, DavError> {
    let rest = if raw_path == DAV_PREFIX {
        ""
    } else {
        raw_path
            .strip_prefix(&format!("{DAV_PREFIX}/"))
            .ok_or_else(DavError::not_found)?
    };
    let mut segments = Vec::new();
    for segment in rest.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = percent_decode_str(segment)
            .decode_utf8()
            .map_err(|_| DavError::bad_request("invalid path encoding"))?
            .to_string();
        if decoded == "." || decoded == ".." || decoded.contains('/') || decoded.contains('\0') {
            return Err(DavError::bad_request("invalid path"));
        }
        segments.push(decoded);
    }
    Ok(segments)
}

fn resolve_target(grant: &DeviceGrant, segments: &[String]) -> Result<Target, DavError> {
    let Some(vault) = segments.first() else {
        return Ok(Target::Ancestor {
            href: format!("{DAV_PREFIX}/"),
            name: "dav".to_string(),
            child_name: grant.vault.clone(),
            child_href: href_for(&grant.vault, "", true),
        });
    };
    if vault != &grant.vault {
        return Err(DavError::forbidden("forbidden: vault is not accessible"));
    }
    let relative = segments[1..].join("/");
    let folder = grant.folder.as_str();
    if relative == folder || relative.starts_with(&format!("{folder}/")) {
        return Ok(Target::Inside { path: relative });
    }
    let is_ancestor = relative.is_empty() || folder.starts_with(&format!("{relative}/"));
    if !is_ancestor {
        return Err(DavError::forbidden(
            "forbidden: path is outside the folder granted to this device password",
        ));
    }
    let remaining = if relative.is_empty() {
        folder
    } else {
        &folder[relative.len() + 1..]
    };
    let child_name = remaining.split('/').next().unwrap_or(remaining).to_string();
    let child_path = if relative.is_empty() {
        child_name.clone()
    } else {
        format!("{relative}/{child_name}")
    };
    Ok(Target::Ancestor {
        href: href_for(&grant.vault, &relative, true),
        name: if relative.is_empty() {
            grant.vault.clone()
        } else {
            relative.rsplit('/').next().unwrap_or(&relative).to_string()
        },
        child_name,
        child_href: href_for(&grant.vault, &child_path, true),
    })
}

fn href_for(vault: &str, path: &str, is_dir: bool) -> String {
    let mut href = format!("{DAV_PREFIX}/{}", encode_path_segment(vault));
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        href.push('/');
        href.push_str(&encode_path_segment(segment));
    }
    if is_dir {
        href.push('/');
    }
    href
}

fn device_for(grant: &DeviceGrant) -> DavDevice {
    DavDevice {
        client_id: format!("webdav-{}", grant.id),
        name: grant.label.clone(),
    }
}

fn inside_path(target: &Target) -> Result<&str, DavError> {
    match target {
        Target::Inside { path } => Ok(path),
        Target::Ancestor { .. } => Err(DavError::forbidden(
            "forbidden: only the granted folder is writable",
        )),
    }
}

/// The granted folder itself always exists as a collection, even before any file lands in it.
async fn stat_or_virtual(
    state: &AppState,
    grant: &DeviceGrant,
    path: &str,
) -> Result<Option<DavEntry>, DavError> {
    let entry = state
        .vaults
        .dav_stat(&grant.user, &grant.vault, path)
        .await?;
    if entry.is_none() && path == grant.folder {
        return Ok(Some(DavEntry::dir(path, unix_now_millis())));
    }
    Ok(entry)
}

async fn propfind(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
    headers: &HeaderMap,
) -> Result<Response, DavError> {
    let depth = match headers
        .get("depth")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("0") => 0,
        Some("1") | None => 1,
        Some("infinity") => {
            return Ok((
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:"><D:propfind-finite-depth/></D:error>"#,
            )
                .into_response());
        }
        Some(_) => return Err(DavError::bad_request("invalid Depth header")),
    };

    let now = unix_now_millis();
    let mut responses = Vec::new();
    match target {
        Target::Ancestor {
            href,
            name,
            child_name,
            child_href,
        } => {
            responses.push(render_response(href, name, &DavEntry::dir("", now)));
            if depth == 1 {
                responses.push(render_response(
                    child_href,
                    child_name,
                    &DavEntry::dir("", now),
                ));
            }
        }
        Target::Inside { path } => {
            let entry = stat_or_virtual(state, grant, path)
                .await?
                .ok_or_else(DavError::not_found)?;
            let name = if path.is_empty() {
                grant.vault.clone()
            } else {
                entry.name().to_string()
            };
            responses.push(render_response(
                &href_for(&grant.vault, path, entry.is_dir),
                &name,
                &entry,
            ));
            if depth == 1 && entry.is_dir {
                for child in state
                    .vaults
                    .dav_list(&grant.user, &grant.vault, path)
                    .await?
                {
                    responses.push(render_response(
                        &href_for(&grant.vault, &child.path, child.is_dir),
                        child.name(),
                        &child,
                    ));
                }
            }
        }
    }

    Ok(multistatus(responses))
}

fn multistatus(responses: Vec<String>) -> Response {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">{}</D:multistatus>"#,
        responses.join("")
    );
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

fn render_response(href: &str, name: &str, entry: &DavEntry) -> String {
    let seconds = unix_seconds_from_millis(entry.mtime_millis);
    let mut props = format!(
        "<D:displayname>{}</D:displayname><D:getlastmodified>{}</D:getlastmodified><D:creationdate>{}</D:creationdate>",
        escape_xml(name),
        http_date_from_unix(seconds),
        rfc3339_from_unix(seconds),
    );
    if entry.is_dir {
        props.push_str("<D:resourcetype><D:collection/></D:resourcetype>");
    } else {
        props.push_str(&format!(
            "<D:resourcetype/><D:getcontentlength>{}</D:getcontentlength><D:getcontenttype>{}</D:getcontenttype><D:getetag>\"{}\"</D:getetag>",
            entry.size,
            content_type_for(&entry.path),
            escape_xml(&entry.etag),
        ));
    }
    props.push_str(
        "<D:supportedlock><D:lockentry><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry></D:supportedlock>",
    );
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        escape_xml(href)
    )
}

async fn get(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
    headers: &HeaderMap,
    head_only: bool,
) -> Result<Response, DavError> {
    let path = inside_path(target)?;
    let entry = stat_or_virtual(state, grant, path)
        .await?
        .ok_or_else(DavError::not_found)?;
    if entry.is_dir {
        let children = state
            .vaults
            .dav_list(&grant.user, &grant.vault, path)
            .await?;
        let listing = render_directory_listing(&grant.vault, path, &children);
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            if head_only { String::new() } else { listing },
        )
            .into_response());
    }

    let (entry, content) = state
        .vaults
        .dav_read(&grant.user, &grant.vault, path)
        .await?;
    let total = content.len() as u64;
    let range = if head_only {
        None
    } else {
        parse_range(headers.get(header::RANGE), total)?
    };
    let (status, slice) = match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            &content[start as usize..=end as usize],
        ),
        None => (StatusCode::OK, &content[..]),
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type_for(&entry.path))
        .header(header::CONTENT_LENGTH, slice.len())
        .header(
            header::LAST_MODIFIED,
            http_date_from_unix(unix_seconds_from_millis(entry.mtime_millis)),
        )
        .header(header::ETAG, format!("\"{}\"", entry.etag))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store");
    if let Some((start, end)) = range {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(slice.to_vec())
    };
    response.body(body).map_err(DavError::internal)
}

/// Parses a single-range `Range: bytes=...` header into an inclusive `(start, end)`.
/// Malformed or multi-range headers are ignored (the whole file is served, as RFC 9110 allows);
/// syntactically valid ranges that fall outside the file are rejected with 416.
fn parse_range(value: Option<&HeaderValue>, total: u64) -> Result<Option<(u64, u64)>, DavError> {
    let Some(spec) = value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = spec.split_once('-') else {
        return Ok(None);
    };
    let unsatisfiable = || DavError::range_not_satisfiable(total);
    match (start.trim(), end.trim()) {
        ("", "") => Ok(None),
        ("", suffix) => {
            let Ok(length) = suffix.parse::<u64>() else {
                return Ok(None);
            };
            if length == 0 || total == 0 {
                return Err(unsatisfiable());
            }
            Ok(Some((total.saturating_sub(length), total - 1)))
        }
        (start, "") => {
            let Ok(start) = start.parse::<u64>() else {
                return Ok(None);
            };
            if start >= total {
                return Err(unsatisfiable());
            }
            Ok(Some((start, total - 1)))
        }
        (start, end) => {
            let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                return Ok(None);
            };
            if start > end {
                return Ok(None);
            }
            if start >= total {
                return Err(unsatisfiable());
            }
            Ok(Some((start, end.min(total - 1))))
        }
    }
}

async fn put(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, DavError> {
    let path = inside_path(target)?;
    if path == grant.folder {
        return Err(DavError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "cannot PUT onto a collection",
        ));
    }
    if headers.contains_key(header::CONTENT_RANGE) {
        return Err(DavError::bad_request("partial PUT is not supported"));
    }
    let limit = state.webdav_max_body_bytes;
    let declared_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > limit) {
        return Err(DavError::payload_too_large(limit));
    }

    let staged = state
        .vaults
        .dav_stage_upload(&grant.user, &grant.vault)
        .await?;
    if let Err(error) = stream_body_to_file(body, &staged, limit).await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(error);
    }
    let created = state
        .vaults
        .dav_write_from_file(&grant.user, &grant.vault, path, &staged, &device_for(grant))
        .await?;
    Ok(if created {
        StatusCode::CREATED.into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    })
}

async fn delete(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
) -> Result<Response, DavError> {
    let path = inside_path(target)?;
    if path == grant.folder {
        return Err(DavError::forbidden(
            "forbidden: the granted folder itself cannot be deleted",
        ));
    }
    state
        .vaults
        .dav_delete(&grant.user, &grant.vault, path, &device_for(grant))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mkcol(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
    body: &Bytes,
) -> Result<Response, DavError> {
    let path = inside_path(target)?;
    if !body.is_empty() {
        return Err(DavError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "MKCOL with a body is not supported",
        ));
    }
    if path == grant.folder {
        return Err(DavError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "collection already exists",
        ));
    }
    let parent = path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    match stat_or_virtual(state, grant, parent).await? {
        Some(entry) if entry.is_dir => {}
        _ => {
            return Err(DavError::new(
                StatusCode::CONFLICT,
                "parent collection does not exist",
            ))
        }
    }
    state
        .vaults
        .dav_mkcol(&grant.user, &grant.vault, path)
        .await?;
    Ok(StatusCode::CREATED.into_response())
}

async fn move_or_copy(
    state: &AppState,
    grant: &DeviceGrant,
    target: &Target,
    headers: &HeaderMap,
    remove_source: bool,
) -> Result<Response, DavError> {
    let from = inside_path(target)?;
    if from == grant.folder {
        return Err(DavError::forbidden(
            "forbidden: the granted folder itself cannot be moved",
        ));
    }
    let destination = headers
        .get("destination")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| DavError::bad_request("Destination header is required"))?;
    let destination_path = destination_request_path(destination)?;
    let segments = decode_path_segments(&destination_path)?;
    let to = match resolve_target(grant, &segments)? {
        Target::Inside { path } => path,
        Target::Ancestor { .. } => {
            return Err(DavError::forbidden(
                "forbidden: destination is outside the granted folder",
            ))
        }
    };
    if to == grant.folder {
        return Err(DavError::forbidden(
            "forbidden: the granted folder itself cannot be replaced",
        ));
    }
    let overwrite = !headers
        .get("overwrite")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("f"));
    let created = state
        .vaults
        .dav_move_or_copy(
            &grant.user,
            &grant.vault,
            from,
            &to,
            overwrite,
            remove_source,
            &device_for(grant),
        )
        .await?;
    Ok(if created {
        StatusCode::CREATED.into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    })
}

fn destination_request_path(destination: &str) -> Result<String, DavError> {
    let trimmed = destination.trim();
    if trimmed.starts_with('/') {
        return Ok(trimmed.to_string());
    }
    let url = url::Url::parse(trimmed)
        .map_err(|_| DavError::bad_request("Destination header is not a valid URL"))?;
    Ok(url.path().to_string())
}

fn lock(grant: &DeviceGrant, target: &Target, headers: &HeaderMap) -> Result<Response, DavError> {
    // Advisory only: the token is never enforced, but returning one lets class 2 clients
    // (macOS Finder, Windows Explorer, some sync apps) proceed with their lock-then-write flow.
    let path = inside_path(target)?;
    let token = format!("opaquelocktoken:{}", random_lock_token()?);
    let timeout = headers
        .get("timeout")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .filter_map(|part| part.trim().strip_prefix("Second-"))
                .filter_map(|seconds| seconds.parse::<u64>().ok())
                .next()
        })
        .map(|seconds| seconds.min(LOCK_TIMEOUT_SECONDS))
        .unwrap_or(LOCK_TIMEOUT_SECONDS);
    let href = href_for(&grant.vault, path, false);
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope><D:depth>0</D:depth><D:timeout>Second-{timeout}</D:timeout><D:locktoken><D:href>{token}</D:href></D:locktoken><D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock></D:lockdiscovery></D:prop>"#,
        escape_xml(&href)
    );
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/xml; charset=utf-8".to_string(),
            ),
            (
                header::HeaderName::from_static("lock-token"),
                format!("<{token}>"),
            ),
        ],
        body,
    )
        .into_response())
}

fn proppatch(grant: &DeviceGrant, target: &Target) -> Result<Response, DavError> {
    // Property changes (typically a client trying to set the modification time) are accepted
    // and ignored; the vault's own metadata is authoritative.
    let path = inside_path(target)?;
    let href = href_for(&grant.vault, path, false);
    Ok(multistatus(vec![format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        escape_xml(&href)
    )]))
}

fn random_lock_token() -> Result<String, DavError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| DavError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn render_directory_listing(vault: &str, path: &str, children: &[DavEntry]) -> String {
    let items = children
        .iter()
        .map(|child| {
            format!(
                r#"<li><a href="{}">{}{}</a></li>"#,
                escape_xml(&href_for(vault, &child.path, child.is_dir)),
                escape_xml(child.name()),
                if child.is_dir { "/" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><title>{}</title></head><body><h1>{}</h1><ul>{items}</ul></body></html>"#,
        escape_xml(path),
        escape_xml(path),
    )
}

pub fn content_type_for(path: &str) -> &'static str {
    let lower = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match lower.rsplit('.').next().unwrap_or("") {
        "pdf" => "application/pdf",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "zip" => "application/zip",
        "epub" => "application/epub+zip",
        _ => "application/octet-stream",
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> DeviceGrant {
        DeviceGrant {
            id: "abc".to_string(),
            user: "alice".to_string(),
            vault: "notes".to_string(),
            folder: "Tablet/Notes".to_string(),
            label: "Boox".to_string(),
        }
    }

    #[test]
    fn resolves_ancestors_inside_and_outside_paths() {
        let grant = grant();
        assert!(matches!(
            resolve_target(&grant, &[]).unwrap(),
            Target::Ancestor { child_name, .. } if child_name == "notes"
        ));
        assert!(matches!(
            resolve_target(&grant, &["notes".to_string()]).unwrap(),
            Target::Ancestor { child_name, child_href, .. }
                if child_name == "Tablet" && child_href == "/dav/notes/Tablet/"
        ));
        assert!(matches!(
            resolve_target(&grant, &["notes".to_string(), "Tablet".to_string()]).unwrap(),
            Target::Ancestor { child_name, .. } if child_name == "Notes"
        ));
        assert_eq!(
            resolve_target(
                &grant,
                &[
                    "notes".to_string(),
                    "Tablet".to_string(),
                    "Notes".to_string()
                ]
            )
            .unwrap(),
            Target::Inside {
                path: "Tablet/Notes".to_string()
            }
        );
        assert_eq!(
            resolve_target(
                &grant,
                &[
                    "notes".to_string(),
                    "Tablet".to_string(),
                    "Notes".to_string(),
                    "a.pdf".to_string()
                ]
            )
            .unwrap(),
            Target::Inside {
                path: "Tablet/Notes/a.pdf".to_string()
            }
        );
        assert!(resolve_target(&grant, &["other".to_string()]).is_err());
        assert!(resolve_target(&grant, &["notes".to_string(), "Private".to_string()]).is_err());
        assert!(resolve_target(
            &grant,
            &[
                "notes".to_string(),
                "Tablet".to_string(),
                "Notes2".to_string()
            ]
        )
        .is_err());
    }

    #[test]
    fn decodes_and_rejects_paths() {
        assert_eq!(
            decode_path_segments("/dav/notes/My%20Folder/a.pdf").unwrap(),
            vec!["notes", "My Folder", "a.pdf"]
        );
        assert_eq!(decode_path_segments("/dav").unwrap(), Vec::<String>::new());
        assert_eq!(decode_path_segments("/dav/").unwrap(), Vec::<String>::new());
        assert!(decode_path_segments("/dav/notes/..%2Fx").is_err());
        assert!(decode_path_segments("/dav/notes/../x").is_err());
        assert!(decode_path_segments("/other").is_err());
    }

    #[test]
    fn parses_basic_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("alice:pa:ss"))
                .parse()
                .unwrap(),
        );
        assert_eq!(
            basic_credentials(&headers),
            Some(("alice".to_string(), "pa:ss".to_string()))
        );
        headers.insert(header::AUTHORIZATION, "Bearer x".parse().unwrap());
        assert_eq!(basic_credentials(&headers), None);
    }

    #[test]
    fn parses_single_byte_ranges() {
        let header = |value: &str| HeaderValue::from_str(value).unwrap();
        assert_eq!(parse_range(None, 100).unwrap(), None);
        assert_eq!(
            parse_range(Some(&header("bytes=10-19")), 100).unwrap(),
            Some((10, 19))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=90-")), 100).unwrap(),
            Some((90, 99))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=-5")), 100).unwrap(),
            Some((95, 99))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=0-500")), 100).unwrap(),
            Some((0, 99))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=0-1,5-6")), 100).unwrap(),
            None
        );
        assert_eq!(parse_range(Some(&header("items=1-2")), 100).unwrap(), None);
        assert!(parse_range(Some(&header("bytes=200-300")), 100).is_err());
        assert!(parse_range(Some(&header("bytes=0-")), 0).is_err());
    }

    #[test]
    fn client_ip_prefers_forwarded_header() {
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(client_ip(&headers, Some(peer)), "192.0.2.1");
        assert_eq!(client_ip(&headers, None), "unknown");
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers, Some(peer)), "203.0.113.9");
    }

    #[test]
    fn destination_accepts_urls_and_paths() {
        assert_eq!(
            destination_request_path("https://sync.example.com/dav/notes/Tablet/b.pdf").unwrap(),
            "/dav/notes/Tablet/b.pdf"
        );
        assert_eq!(
            destination_request_path("/dav/notes/Tablet/b.pdf").unwrap(),
            "/dav/notes/Tablet/b.pdf"
        );
        assert!(destination_request_path("not a url").is_err());
    }
}
