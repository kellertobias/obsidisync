use axum::body::{to_bytes, Body};
use axum::http::{header, Request, Response, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use obsidian_git_sync_server::auth::AuthVerifier;
use obsidian_git_sync_server::auth_throttle::IP_FAILURE_LIMIT;
use obsidian_git_sync_server::http::{
    router, router_with_webdav_limit, AppState, PublicAuthConfig,
};
use obsidian_git_sync_server::protocol::{
    FileContentMode, RegisterRequest, ServerFileChange, SyncRequest,
};
use obsidian_git_sync_server::vault::VaultService;
use serde_json::Value;
use tower::ServiceExt;

const BEARER: &str = "Bearer secret";

fn state(root: &std::path::Path) -> AppState {
    AppState::new(
        VaultService::new(root.join("data")),
        AuthVerifier::StaticTokenForDev {
            token: "secret".to_string(),
            user: "alice".to_string(),
        },
        PublicAuthConfig::Token,
    )
}

fn app(root: &std::path::Path) -> axum::Router {
    router(state(root), 1024 * 1024, Vec::new())
}

async fn register(app: &axum::Router) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/register")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&RegisterRequest {
                        remote_url: String::new(),
                        branch: "main".to_string(),
                        author_name: "Test".to_string(),
                        author_email: "test@example.invalid".to_string(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

async fn create_password(app: &axum::Router, label: &str, folder: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/device-passwords")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "label": label, "folder": folder }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await
}

fn basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    )
}

async fn dav(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        builder = builder.header("authorization", auth);
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn text(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn sync_files(app: &axum::Router, base_head: Option<&str>) -> (Value, Vec<ServerFileChange>) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/sync")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&SyncRequest {
                        base_head: base_head.map(ToString::to_string),
                        client_id: "phone".to_string(),
                        device_name: "iPhone".to_string(),
                        changes: vec![],
                        client_manifest: vec![],
                        file_content: Default::default(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    let files: Vec<ServerFileChange> = serde_json::from_value(body["files"].clone()).unwrap();
    (body, files)
}

#[tokio::test]
async fn device_password_management_requires_bearer_auth_and_registered_vault() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/users/alice/vaults/notes/device-passwords")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let before_registration = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/device-passwords")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "label": "Boox", "folder": "Tablet" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(before_registration.status(), StatusCode::BAD_REQUEST);
    let error = json(before_registration).await;
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("sync this vault from Obsidian once"));

    register(&app).await;

    let created = create_password(&app, "Boox tablet", "/Tablet/Notes/").await;
    assert_eq!(created["username"], "alice");
    assert_eq!(created["vault"], "notes");
    assert_eq!(created["folder"], "Tablet/Notes");
    assert_eq!(created["webdavPath"], "/dav/notes/Tablet/Notes/");
    assert_eq!(created["label"], "Boox tablet");
    let password = created["password"].as_str().unwrap();
    assert_eq!(password.len(), 24);

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/users/alice/vaults/notes/device-passwords")
                .header("authorization", BEARER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = json(listed).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(listed[0].get("password").is_none());
    assert_eq!(listed[0]["id"], created["id"]);

    let invalid_folder = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/device-passwords")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "label": "Bad", "folder": "../outside" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_folder.status(), StatusCode::BAD_REQUEST);

    let revoke = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/v1/users/alice/vaults/notes/device-passwords/{}",
                    created["id"].as_str().unwrap()
                ))
                .header("authorization", BEARER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::OK);

    let revoke_again = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/v1/users/alice/vaults/notes/device-passwords/{}",
                    created["id"].as_str().unwrap()
                ))
                .header("authorization", BEARER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke_again.status(), StatusCode::NOT_FOUND);

    let after_revoke = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(&basic("alice", password)),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webdav_authenticates_with_device_passwords_and_scopes_to_folder() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet/Notes").await;
    let password = created["password"].as_str().unwrap();
    let auth = basic("alice", password);

    let options = dav(
        &app,
        "OPTIONS",
        "/dav/notes/Tablet/Notes/",
        None,
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(options.status(), StatusCode::OK);
    assert_eq!(options.headers().get("dav").unwrap(), "1, 2");
    assert!(options
        .headers()
        .get(header::ALLOW)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("PROPFIND"));

    let missing = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        None,
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert!(missing
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("Basic realm="));

    let wrong = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(&basic("alice", "wrong-password")),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let bearer_is_not_accepted = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(BEARER),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(bearer_is_not_accepted.status(), StatusCode::UNAUTHORIZED);

    // Outside the granted folder, other vaults, and the vault root are off limits for writes.
    for uri in [
        "/dav/notes/Private/secret.md",
        "/dav/other/Tablet/Notes/a.pdf",
        "/dav/notes/Tablet/Notes2/a.pdf",
        "/dav/notes/root.md",
    ] {
        let put = dav(&app, "PUT", uri, Some(&auth), &[], b"x".to_vec()).await;
        assert_eq!(put.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    let get_root = dav(&app, "GET", "/dav/notes/", Some(&auth), &[], Vec::new()).await;
    assert_eq!(get_root.status(), StatusCode::FORBIDDEN);

    // Ancestors are browsable and only expose the way towards the granted folder.
    let root_listing = dav(
        &app,
        "PROPFIND",
        "/dav/",
        Some(&auth),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert_eq!(root_listing.status(), StatusCode::MULTI_STATUS);
    let body = text(root_listing).await;
    assert!(body.contains("<D:href>/dav/</D:href>"));
    assert!(body.contains("<D:href>/dav/notes/</D:href>"));

    let vault_listing = dav(
        &app,
        "PROPFIND",
        "/dav/notes",
        Some(&auth),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert_eq!(vault_listing.status(), StatusCode::MULTI_STATUS);
    let body = text(vault_listing).await;
    assert!(
        body.contains("<D:href>/dav/notes/Tablet/</D:href>"),
        "{body}"
    );
    assert!(!body.contains("Private"));

    // The granted folder exists as an empty collection before any file is uploaded.
    let empty_folder = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(&auth),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert_eq!(empty_folder.status(), StatusCode::MULTI_STATUS);
    let body = text(empty_folder).await;
    assert!(body.contains("<D:href>/dav/notes/Tablet/Notes/</D:href>"));
    assert!(body.contains("<D:collection/>"));
    assert!(body.contains("<D:displayname>Notes</D:displayname>"));

    let infinite = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(&auth),
        &[("depth", "infinity")],
        Vec::new(),
    )
    .await;
    assert_eq!(infinite.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn webdav_uploads_reach_obsidian_clients_through_sync() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox tablet", "Tablet/Notes").await;
    let auth = basic("alice", created["password"].as_str().unwrap());

    let (initial, _) = sync_files(&app, None).await;
    let initial_head = initial["serverHead"].as_str().map(ToString::to_string);

    let pdf = b"%PDF-1.4 fake tablet notes".to_vec();
    let put_pdf = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/Notes/Meeting%20notes.pdf",
        Some(&auth),
        &[("content-type", "application/pdf")],
        pdf.clone(),
    )
    .await;
    assert_eq!(put_pdf.status(), StatusCode::CREATED);

    let put_md = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/Notes/index.md",
        Some(&auth),
        &[],
        b"# Tablet index\n".to_vec(),
    )
    .await;
    assert_eq!(put_md.status(), StatusCode::CREATED);

    let overwrite = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/Notes/index.md",
        Some(&auth),
        &[],
        b"# Tablet index v2\n".to_vec(),
    )
    .await;
    assert_eq!(overwrite.status(), StatusCode::NO_CONTENT);

    let listing = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/Notes/",
        Some(&auth),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert_eq!(listing.status(), StatusCode::MULTI_STATUS);
    let body = text(listing).await;
    assert!(
        body.contains("<D:href>/dav/notes/Tablet/Notes/Meeting%20notes.pdf</D:href>"),
        "{body}"
    );
    assert!(body.contains("<D:getcontenttype>application/pdf</D:getcontenttype>"));
    assert!(body.contains(&format!(
        "<D:getcontentlength>{}</D:getcontentlength>",
        pdf.len()
    )));
    assert!(body.contains("<D:href>/dav/notes/Tablet/Notes/index.md</D:href>"));

    let get_pdf = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/Notes/Meeting%20notes.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(get_pdf.status(), StatusCode::OK);
    assert_eq!(
        get_pdf.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
    assert_eq!(
        to_bytes(get_pdf.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        pdf
    );

    let head_pdf = dav(
        &app,
        "HEAD",
        "/dav/notes/Tablet/Notes/Meeting%20notes.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(head_pdf.status(), StatusCode::OK);
    assert_eq!(
        head_pdf.headers().get(header::CONTENT_LENGTH).unwrap(),
        &pdf.len().to_string()
    );

    let missing = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/Notes/nope.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // An Obsidian client syncing from the previous head receives both files.
    let (synced, files) = sync_files(&app, initial_head.as_deref()).await;
    assert_eq!(synced["status"], "ok");
    let mut paths: Vec<&str> = files
        .iter()
        .map(|file| match file {
            ServerFileChange::Upsert { path, .. } | ServerFileChange::Delete { path } => {
                path.as_str()
            }
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["Tablet/Notes/Meeting notes.pdf", "Tablet/Notes/index.md"]
    );
    let pdf_from_sync = files
        .iter()
        .find_map(|file| match file {
            ServerFileChange::Upsert {
                path,
                content_base64,
                ..
            } if path == "Tablet/Notes/Meeting notes.pdf" => content_base64.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(STANDARD.decode(pdf_from_sync).unwrap(), pdf);

    // Re-uploading identical bytes is a no-op: no new commit, nothing for clients to fetch.
    let (after_first, _) = sync_files(&app, None).await;
    let head_before = after_first["serverHead"].as_str().unwrap().to_string();
    let repeat = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/Notes/Meeting%20notes.pdf",
        Some(&auth),
        &[],
        pdf.clone(),
    )
    .await;
    assert_eq!(repeat.status(), StatusCode::NO_CONTENT);
    let (after_repeat, unchanged) = sync_files(&app, Some(&head_before)).await;
    assert_eq!(after_repeat["serverHead"].as_str().unwrap(), head_before);
    assert!(unchanged.is_empty(), "{unchanged:?}");

    // The tablet shows up as a sync device with its label.
    let history = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/users/alice/vaults/notes/history?path=Tablet/Notes/index.md")
                .header("authorization", BEARER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let history = json(history).await;
    assert_eq!(history[0]["deviceName"], "Boox tablet");

    let devices = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/users/alice/vaults/notes/devices")
                .header("authorization", BEARER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let devices = json(devices).await;
    assert!(devices
        .as_array()
        .unwrap()
        .iter()
        .any(|device| device["deviceName"] == "Boox tablet"));
}

#[tokio::test]
async fn webdav_supports_collections_moves_deletes_and_locks() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet").await;
    let auth = basic("alice", created["password"].as_str().unwrap());

    let mkcol = dav(
        &app,
        "MKCOL",
        "/dav/notes/Tablet/Archive/",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(mkcol.status(), StatusCode::CREATED);
    let mkcol_again = dav(
        &app,
        "MKCOL",
        "/dav/notes/Tablet/Archive/",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(mkcol_again.status(), StatusCode::METHOD_NOT_ALLOWED);
    let mkcol_orphan = dav(
        &app,
        "MKCOL",
        "/dav/notes/Tablet/a/b/",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(mkcol_orphan.status(), StatusCode::CONFLICT);

    let listing = text(
        dav(
            &app,
            "PROPFIND",
            "/dav/notes/Tablet/",
            Some(&auth),
            &[("depth", "1")],
            Vec::new(),
        )
        .await,
    )
    .await;
    assert!(
        listing.contains("<D:href>/dav/notes/Tablet/Archive/</D:href>"),
        "{listing}"
    );

    let put = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/draft.pdf",
        Some(&auth),
        &[],
        b"%PDF draft".to_vec(),
    )
    .await;
    assert_eq!(put.status(), StatusCode::CREATED);

    let lock = dav(
        &app,
        "LOCK",
        "/dav/notes/Tablet/draft.pdf",
        Some(&auth),
        &[("timeout", "Second-600")],
        Vec::new(),
    )
    .await;
    assert_eq!(lock.status(), StatusCode::OK);
    let token = lock
        .headers()
        .get("lock-token")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(token.starts_with("<opaquelocktoken:"));
    let lock_body = text(lock).await;
    assert!(lock_body.contains("<D:timeout>Second-600</D:timeout>"));
    let unlock = dav(
        &app,
        "UNLOCK",
        "/dav/notes/Tablet/draft.pdf",
        Some(&auth),
        &[("lock-token", &token)],
        Vec::new(),
    )
    .await;
    assert_eq!(unlock.status(), StatusCode::NO_CONTENT);

    let moved = dav(
        &app,
        "MOVE",
        "/dav/notes/Tablet/draft.pdf",
        Some(&auth),
        &[(
            "destination",
            "https://sync.example.com/dav/notes/Tablet/Archive/final.pdf",
        )],
        Vec::new(),
    )
    .await;
    assert_eq!(moved.status(), StatusCode::CREATED);
    let old = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/draft.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(old.status(), StatusCode::NOT_FOUND);
    let new = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/Archive/final.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(new.status(), StatusCode::OK);
    assert_eq!(text(new).await, "%PDF draft");

    let move_outside = dav(
        &app,
        "MOVE",
        "/dav/notes/Tablet/Archive/final.pdf",
        Some(&auth),
        &[("destination", "/dav/notes/Private/final.pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(move_outside.status(), StatusCode::FORBIDDEN);

    let copied = dav(
        &app,
        "COPY",
        "/dav/notes/Tablet/Archive/final.pdf",
        Some(&auth),
        &[("destination", "/dav/notes/Tablet/copy.pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(copied.status(), StatusCode::CREATED);
    let no_overwrite = dav(
        &app,
        "COPY",
        "/dav/notes/Tablet/Archive/final.pdf",
        Some(&auth),
        &[
            ("destination", "/dav/notes/Tablet/copy.pdf"),
            ("overwrite", "F"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(no_overwrite.status(), StatusCode::PRECONDITION_FAILED);

    let delete_dir = dav(
        &app,
        "DELETE",
        "/dav/notes/Tablet/Archive/",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(delete_dir.status(), StatusCode::NO_CONTENT);
    let gone = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/Archive/final.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    let delete_root = dav(
        &app,
        "DELETE",
        "/dav/notes/Tablet/",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(delete_root.status(), StatusCode::FORBIDDEN);

    let (_, files) = sync_files(&app, None).await;
    let paths: Vec<String> = files
        .iter()
        .map(|file| match file {
            ServerFileChange::Upsert { path, .. } | ServerFileChange::Delete { path } => {
                path.clone()
            }
        })
        .collect();
    assert_eq!(paths, vec!["Tablet/copy.pdf".to_string()]);

    let unknown = dav(
        &app,
        "PATCH",
        "/dav/notes/Tablet/copy.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn webdav_throttles_repeated_failed_logins_per_client() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet").await;
    let good = basic("alice", created["password"].as_str().unwrap());
    let bad = basic("alice", "nope-nope-nope-nope-nope");

    for _ in 0..IP_FAILURE_LIMIT {
        let response = dav(
            &app,
            "PROPFIND",
            "/dav/notes/Tablet/",
            Some(&bad),
            &[("depth", "0")],
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // Same client, even with the right password: locked out with a Retry-After hint.
    let locked = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/",
        Some(&good),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = locked
        .headers()
        .get(header::RETRY_AFTER)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after > 0);

    // A different client address (here via X-Forwarded-For) is unaffected.
    let other_client = dav(
        &app,
        "PROPFIND",
        "/dav/notes/Tablet/",
        Some(&good),
        &[("depth", "0"), ("x-forwarded-for", "203.0.113.7")],
        Vec::new(),
    )
    .await;
    assert_eq!(other_client.status(), StatusCode::MULTI_STATUS);
}

#[tokio::test]
async fn webdav_rejects_uploads_over_the_configured_limit() {
    let root = tempfile::tempdir().unwrap();
    let app = router_with_webdav_limit(state(root.path()), 1024 * 1024, 64, Vec::new());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet").await;
    let auth = basic("alice", created["password"].as_str().unwrap());

    let small = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/small.pdf",
        Some(&auth),
        &[],
        vec![1; 64],
    )
    .await;
    assert_eq!(small.status(), StatusCode::CREATED);

    let too_big = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/big.pdf",
        Some(&auth),
        &[],
        vec![1; 65],
    )
    .await;
    assert_eq!(too_big.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let missing = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/big.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // Nothing is left behind in the staging area after a rejected upload.
    let mut uploads =
        tokio::fs::read_dir(root.path().join("data/users/alice/vaults/notes/uploads"))
            .await
            .unwrap();
    assert!(uploads.next_entry().await.unwrap().is_none());
}

#[tokio::test]
async fn webdav_serves_byte_ranges() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet").await;
    let auth = basic("alice", created["password"].as_str().unwrap());
    let content: Vec<u8> = (0..100).collect();
    let put = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/doc.pdf",
        Some(&auth),
        &[],
        content.clone(),
    )
    .await;
    assert_eq!(put.status(), StatusCode::CREATED);

    let full = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/doc.pdf",
        Some(&auth),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");

    let partial = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/doc.pdf",
        Some(&auth),
        &[("range", "bytes=10-19")],
        Vec::new(),
    )
    .await;
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        partial.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-19/100"
    );
    assert_eq!(partial.headers().get(header::CONTENT_LENGTH).unwrap(), "10");
    assert_eq!(
        to_bytes(partial.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        content[10..20].to_vec()
    );

    let tail = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/doc.pdf",
        Some(&auth),
        &[("range", "bytes=-5")],
        Vec::new(),
    )
    .await;
    assert_eq!(tail.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(tail.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        content[95..].to_vec()
    );

    let beyond = dav(
        &app,
        "GET",
        "/dav/notes/Tablet/doc.pdf",
        Some(&auth),
        &[("range", "bytes=200-300")],
        Vec::new(),
    )
    .await;
    assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        beyond.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes */100"
    );
}

#[tokio::test]
async fn sync_reference_mode_returns_metadata_and_blob_endpoint_serves_bytes() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    register(&app).await;
    let created = create_password(&app, "Boox", "Tablet").await;
    let auth = basic("alice", created["password"].as_str().unwrap());
    let pdf: Vec<u8> = (0..=255).cycle().take(70_000).collect();
    let put = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/big.pdf",
        Some(&auth),
        &[],
        pdf.clone(),
    )
    .await;
    assert_eq!(put.status(), StatusCode::CREATED);
    let put = dav(
        &app,
        "PUT",
        "/dav/notes/Tablet/note.md",
        Some(&auth),
        &[],
        b"# hi\n".to_vec(),
    )
    .await;
    assert_eq!(put.status(), StatusCode::CREATED);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/notes/sync")
                .header("authorization", BEARER)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&SyncRequest {
                        base_head: None,
                        client_id: "tablet".to_string(),
                        device_name: "Android".to_string(),
                        changes: vec![],
                        client_manifest: vec![],
                        file_content: FileContentMode::Reference,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    let head = body["serverHead"].as_str().unwrap().to_string();
    let files = body["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let big = files
        .iter()
        .find(|file| file["path"] == "Tablet/big.pdf")
        .unwrap();
    assert!(big.get("contentBase64").is_none(), "{big}");
    assert_eq!(big["size"], 70_000);
    let expected_sha = obsidian_git_sync_server::binary_store::sha256_hex(&pdf);
    assert_eq!(big["sha256"], expected_sha);
    let note = files
        .iter()
        .find(|file| file["path"] == "Tablet/note.md")
        .unwrap();
    assert!(note.get("contentBase64").is_none());
    assert_eq!(note["size"], 5);

    for (path, expected) in [
        ("Tablet/big.pdf", pdf.clone()),
        ("Tablet/note.md", b"# hi\n".to_vec()),
    ] {
        let blob = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/v1/users/alice/vaults/notes/blob?path={}&hash={head}",
                        path.replace('/', "%2F")
                    ))
                    .header("authorization", BEARER)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blob.status(), StatusCode::OK, "{path}");
        assert_eq!(
            blob.headers().get("x-content-sha256").unwrap(),
            &obsidian_git_sync_server::binary_store::sha256_hex(&expected)
        );
        assert_eq!(
            to_bytes(blob.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
            expected
        );
    }

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/users/alice/vaults/notes/blob?path=Tablet%2Fbig.pdf&hash={head}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // Inline remains the default for clients that do not send the field.
    let (_, inline_files) = sync_files(&app, None).await;
    assert!(inline_files.iter().all(|file| matches!(
        file,
        ServerFileChange::Upsert {
            content_base64: Some(_),
            ..
        }
    )));
}
