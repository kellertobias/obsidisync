use axum::body::{to_bytes, Body};
use axum::http::{header, Request, Response, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use obsidian_git_sync_server::auth::{normalize_user_claim, AuthVerifier};
use obsidian_git_sync_server::git::git;
use obsidian_git_sync_server::http::{router, AppState, PublicAuthConfig};
use obsidian_git_sync_server::paths::{is_text_or_code_path, validate_vault_path};
use obsidian_git_sync_server::protocol::{
    ClientChange, ManifestEntry, RegisterRequest, ResolveRequest, ResolvedFile, SyncRequest,
    SyncStatus, UploadChunkRequest, UploadInitRequest,
};
use obsidian_git_sync_server::vault::VaultService;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use tokio::fs;
use tower::ServiceExt;

const USER: &str = "alice";
const VAULT: &str = "notes";

#[test]
fn validates_paths_and_user_claims() {
    assert_eq!(normalize_user_claim("Alice@example.com").unwrap(), "alice");
    assert_eq!(
        validate_vault_path("Folder/Note.md").unwrap(),
        "Folder/Note.md"
    );
    assert!(validate_vault_path("../escape.md").is_err());
    assert!(validate_vault_path(".git/config").is_err());
    assert!(is_text_or_code_path("Note.md"));
    assert!(is_text_or_code_path("src/main.rs"));
    assert!(!is_text_or_code_path("photo.png"));

    let verifier = AuthVerifier::StaticTokenForDev {
        token: "secret".to_string(),
        user: "alice".to_string(),
    };
    drop(verifier);
}

#[test]
fn protocol_matches_plugin_json_field_names() {
    let change: ClientChange = serde_json::from_value(serde_json::json!({
        "op": "upsert",
        "path": "Note.md",
        "contentBase64": "aGVsbG8=",
        "sha256": "abc",
        "mtime": 123
    }))
    .unwrap();

    match change {
        ClientChange::Upsert {
            path,
            content_base64,
            upload_id,
            sha256,
            mtime,
        } => {
            assert_eq!(path, "Note.md");
            assert_eq!(content_base64.as_deref(), Some("aGVsbG8="));
            assert_eq!(upload_id, None);
            assert_eq!(sha256.as_deref(), Some("abc"));
            assert_eq!(mtime, Some(123));
        }
        ClientChange::Delete { .. } => panic!("expected upsert"),
    }

    let response = serde_json::to_value(
        obsidian_git_sync_server::protocol::ServerFileChange::Upsert {
            path: "Note.md".to_string(),
            content_base64: "aGVsbG8=".to_string(),
            sha256: "abc".to_string(),
        },
    )
    .unwrap();
    assert_eq!(response["contentBase64"], "aGVsbG8=");
}

#[tokio::test]
async fn server_info_reports_api_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let app = router(
        AppState {
            vaults: VaultService::new(root.path().join("data")),
            auth: AuthVerifier::StaticTokenForDev {
                token: "secret".to_string(),
                user: "alice".to_string(),
            },
            public_auth: PublicAuthConfig::Token,
        },
        1024 * 1024,
        Vec::new(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/server/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["name"], "obsidisync-server");
    assert_eq!(body["apiVersion"], 1);
    assert_eq!(body["minClientApiVersion"], 1);
    assert!(body["version"].as_str().is_some());
}

#[tokio::test]
async fn http_authorization_rejects_cross_user_access() {
    let root = tempfile::tempdir().unwrap();
    let app = router(
        AppState {
            vaults: VaultService::new(root.path().join("data")),
            auth: AuthVerifier::StaticTokenForDev {
                token: "secret".to_string(),
                user: "alice".to_string(),
            },
            public_auth: PublicAuthConfig::Token,
        },
        1024 * 1024,
        Vec::new(),
    );

    let missing_auth = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/personal/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&register_request(Path::new("/tmp/remote.git"))).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_auth.status(), StatusCode::UNAUTHORIZED);

    let wrong_user = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/vaults/personal/register")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&register_request(Path::new("/tmp/remote.git"))).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_user.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn password_auth_page_setup_login_and_authorizes_api_requests() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    let app = router(
        AppState {
            vaults: VaultService::new(data_dir.clone()),
            auth: AuthVerifier::password("Alice@example.com".to_string(), data_dir).unwrap(),
            public_auth: PublicAuthConfig::Password,
        },
        1024 * 1024,
        Vec::new(),
    );

    let setup_page = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup_page.status(), StatusCode::OK);
    assert!(response_text(setup_page).await.contains("Set password"));

    let config_before_setup = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(config_before_setup.status(), StatusCode::OK);
    let config_body = response_json(config_before_setup).await;
    assert_eq!(config_body["type"], "password");
    assert_eq!(config_body["passwordConfigured"], false);

    let setup_form = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "username=alice&password=correct-horse-battery-staple&password_confirm=correct-horse-battery-staple",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup_form.status(), StatusCode::OK);
    let setup_html = response_text(setup_form).await;
    assert!(setup_html.contains("Access token"), "{setup_html}");
    assert!(setup_html.contains("textarea"));

    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "correct-horse-battery-staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let login_body = response_json(login).await;
    assert_eq!(login_body["user"], "alice");
    let token = login_body["accessToken"].as_str().unwrap();
    let refresh_token = login_body["refreshToken"].as_str().unwrap();
    assert!(!token.is_empty());
    assert!(!refresh_token.is_empty());
    assert_eq!(login_body["expiresIn"], 86_400);
    assert_eq!(login_body["refreshExpiresIn"], 15_552_000);

    let refresh = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/session/refresh")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "refreshToken": refresh_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refresh.status(), StatusCode::OK);
    let refresh_body = response_json(refresh).await;
    let token = refresh_body["accessToken"].as_str().unwrap();
    let rotated_refresh_token = refresh_body["refreshToken"].as_str().unwrap();
    assert_ne!(token, login_body["accessToken"].as_str().unwrap());
    assert_ne!(rotated_refresh_token, refresh_token);

    let reused_refresh = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/session/refresh")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "refreshToken": refresh_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reused_refresh.status(), StatusCode::UNAUTHORIZED);

    let session = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/auth/session")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session_body = response_json(session).await;
    assert_eq!(session_body["user"], "alice");

    let register_vault = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/personal/register")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&local_register_request()).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_vault.status(), StatusCode::OK);

    let sync_note = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/personal/sync")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&SyncRequest {
                        changes: vec![upsert("Note.md", b"hello feed\n")],
                        ..empty_sync(None)
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_note.status(), StatusCode::OK);

    let login_form = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "username=alice&password=correct-horse-battery-staple",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login_form.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        login_form.headers().get(header::LOCATION).unwrap(),
        "/change-feed"
    );
    let site_cookie = login_form
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        site_cookie.starts_with("obsidisync_session="),
        "{site_cookie}"
    );
    assert!(site_cookie.contains("Path=/"), "{site_cookie}");
    assert!(site_cookie.contains("Max-Age=2592000"), "{site_cookie}");
    assert!(site_cookie.contains("HttpOnly"), "{site_cookie}");
    assert!(site_cookie.contains("SameSite=Strict"), "{site_cookie}");
    assert!(!site_cookie.contains("Secure"), "{site_cookie}");

    let secure_login_form = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-proto", "https")
                .body(Body::from(
                    "username=alice&password=correct-horse-battery-staple",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let secure_site_cookie = secure_login_form
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        secure_site_cookie.contains("Secure"),
        "{secure_site_cookie}"
    );

    let feed_page = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/change-feed")
                .header(header::COOKIE, site_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(feed_page.status(), StatusCode::OK);
    let feed_html = response_text(feed_page).await;
    assert!(feed_html.contains("Change feed"), "{feed_html}");
    assert!(feed_html.contains("Recent changes"), "{feed_html}");
    assert!(feed_html.contains("personal"), "{feed_html}");
    assert!(feed_html.contains("Note.md"), "{feed_html}");

    let unauthenticated_feed_page = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/change-feed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated_feed_page.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        unauthenticated_feed_page
            .headers()
            .get(header::LOCATION)
            .unwrap(),
        "/login"
    );

    let feed_api = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/users/alice/feed")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(feed_api.status(), StatusCode::OK);
    let feed_body = response_json(feed_api).await;
    assert_eq!(feed_body[0]["vault"], "personal");
    assert!(feed_body[0]["files"]
        .as_array()
        .unwrap()
        .contains(&Value::String("Note.md".to_string())));

    let wrong_password = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "wrong-password"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);

    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/personal/register")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&register_request(Path::new("/tmp/remote.git"))).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::BAD_REQUEST);

    let wrong_user = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/vaults/personal/register")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&register_request(Path::new("/tmp/remote.git"))).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_user.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn password_setup_requires_bootstrap_token_when_configured() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    let app = router(
        AppState {
            vaults: VaultService::new(data_dir.clone()),
            auth: AuthVerifier::password_with_setup_token(
                "Alice@example.com".to_string(),
                data_dir,
                Some("setup-token-123456".to_string()),
            )
            .unwrap(),
            public_auth: PublicAuthConfig::Password,
        },
        1024 * 1024,
        Vec::new(),
    );

    let config = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let config_body = response_json(config).await;
    assert_eq!(config_body["setupTokenRequired"], true);

    let setup_without_token = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "correct-horse-battery-staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup_without_token.status(), StatusCode::BAD_REQUEST);

    let setup_with_token = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "correct-horse-battery-staple",
                        "setupToken": "setup-token-123456"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup_with_token.status(), StatusCode::OK);
    let setup_body = response_json(setup_with_token).await;
    assert_eq!(setup_body["user"], "alice");
    let setup_access_token = setup_body["accessToken"].as_str().unwrap();
    let setup_refresh_token = setup_body["refreshToken"].as_str().unwrap();
    assert!(!setup_access_token.is_empty());
    assert!(!setup_refresh_token.is_empty());
    assert_eq!(setup_body["expiresIn"], 86_400);
    assert_eq!(setup_body["refreshExpiresIn"], 15_552_000);

    let refreshed_setup = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/session/refresh")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "refreshToken": setup_refresh_token }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refreshed_setup.status(), StatusCode::OK);
    let refreshed_setup_body = response_json(refreshed_setup).await;
    assert_ne!(
        refreshed_setup_body["accessToken"].as_str().unwrap(),
        setup_access_token
    );
    assert_ne!(
        refreshed_setup_body["refreshToken"].as_str().unwrap(),
        setup_refresh_token
    );
}

#[tokio::test]
async fn password_endpoints_are_disabled_outside_password_mode() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    let app = router(
        AppState {
            vaults: VaultService::new(data_dir.clone()),
            auth: AuthVerifier::password("Alice@example.com".to_string(), data_dir).unwrap(),
            public_auth: PublicAuthConfig::Token,
        },
        1024 * 1024,
        Vec::new(),
    );

    let setup = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "correct-horse-battery-staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::BAD_REQUEST);

    let login = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/password/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "correct-horse-battery-staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_rejects_oversized_sync_bodies() {
    let root = tempfile::tempdir().unwrap();
    let app = router(
        AppState {
            vaults: VaultService::new(root.path().join("data")),
            auth: AuthVerifier::StaticTokenForDev {
                token: "secret".to_string(),
                user: "alice".to_string(),
            },
            public_auth: PublicAuthConfig::Token,
        },
        128,
        Vec::new(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/vaults/personal/sync")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("x".repeat(1024)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn http_does_not_allow_cross_origin_by_default() {
    let root = tempfile::tempdir().unwrap();
    let app = router(
        AppState {
            vaults: VaultService::new(root.path().join("data")),
            auth: AuthVerifier::StaticTokenForDev {
                token: "secret".to_string(),
                user: "alice".to_string(),
            },
            public_auth: PublicAuthConfig::Token,
        },
        1024 * 1024,
        Vec::new(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

#[tokio::test]
async fn production_service_rejects_local_git_remotes() {
    let fixture = GitFixture::new().await;
    let service = VaultService::new(fixture.root.path().join("data"));
    let error = service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("local git remotes are disabled"));
}

#[tokio::test]
async fn syncs_without_remote_using_persistent_server_local_repo() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    let service = VaultService::new(data_dir.clone());

    let registration = service
        .register(USER, VAULT, local_register_request())
        .await
        .unwrap();
    assert_eq!(registration.server_head, None);

    let synced = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                changes: vec![upsert("Note.md", b"local only\n")],
                ..empty_sync(None)
            },
        )
        .await
        .unwrap();
    assert_eq!(synced.status, SyncStatus::Ok);
    let head = synced.server_head.clone().unwrap();

    let repo = data_dir.join("users/alice/vaults/notes/repo");
    assert_eq!(
        fs::read_to_string(repo.join("Note.md")).await.unwrap(),
        "local only\n"
    );
    assert!(git(Some(&repo), &["remote"], &[0])
        .await
        .unwrap()
        .stdout
        .is_empty());

    let restarted = VaultService::new(data_dir);
    let registration_after_restart = restarted
        .register(USER, VAULT, local_register_request())
        .await
        .unwrap();
    assert_eq!(
        registration_after_restart.server_head.as_deref(),
        Some(head.as_str())
    );

    let history = restarted
        .history(USER, VAULT, Some("Note.md"))
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    let version = restarted
        .file_at_version(USER, VAULT, "Note.md", &head)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(STANDARD.decode(version.content_base64).unwrap()).unwrap(),
        "local only\n"
    );
}

#[tokio::test]
async fn syncs_upserts_from_chunked_uploads() {
    let root = tempfile::tempdir().unwrap();
    let service = VaultService::new(root.path().join("data"));
    service
        .register(USER, VAULT, local_register_request())
        .await
        .unwrap();

    let content = b"hello from chunked upload\n";
    let init = service
        .init_upload(
            USER,
            VAULT,
            UploadInitRequest {
                path: "Chunked.md".to_string(),
                sha256: obsidian_git_sync_server::binary_store::sha256_hex(content),
                size: content.len() as u64,
            },
        )
        .await
        .unwrap();
    service
        .append_upload_chunk(
            USER,
            VAULT,
            &init.upload_id,
            UploadChunkRequest {
                offset: 0,
                content_base64: STANDARD.encode(&content[..8]),
            },
        )
        .await
        .unwrap();
    service
        .append_upload_chunk(
            USER,
            VAULT,
            &init.upload_id,
            UploadChunkRequest {
                offset: 8,
                content_base64: STANDARD.encode(&content[8..]),
            },
        )
        .await
        .unwrap();
    let complete = service
        .complete_upload(USER, VAULT, &init.upload_id)
        .await
        .unwrap();
    assert_eq!(complete.size, content.len() as u64);

    let synced = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                changes: vec![ClientChange::Upsert {
                    path: "Chunked.md".to_string(),
                    content_base64: None,
                    upload_id: Some(init.upload_id.clone()),
                    sha256: Some(complete.sha256),
                    mtime: Some(1),
                }],
                client_manifest: vec![ManifestEntry {
                    path: "Chunked.md".to_string(),
                    sha256: obsidian_git_sync_server::binary_store::sha256_hex(content),
                    mtime: 1,
                    size: content.len() as u64,
                }],
                ..empty_sync(None)
            },
        )
        .await
        .unwrap();
    assert_eq!(synced.status, SyncStatus::Ok);
    assert!(
        synced.files.is_empty(),
        "sync should not echo files the client already uploaded"
    );

    let repo = root.path().join("data/users/alice/vaults/notes/repo");
    assert_eq!(fs::read(repo.join("Chunked.md")).await.unwrap(), content);
    assert!(fs::metadata(root.path().join(format!(
        "data/users/alice/vaults/notes/uploads/{}.bin",
        init.upload_id
    )))
    .await
    .is_err());
}

#[tokio::test]
async fn syncs_text_history_and_read_only_file_versions() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));

    let registration = service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    let first = service.sync(USER, VAULT, empty_sync(None)).await.unwrap();
    assert_eq!(first.status, SyncStatus::Ok);
    assert_eq!(file_text(&first.files, "Note.md"), "hello\n");

    let base = first.server_head.clone();
    let edited = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: base.clone(),
                changes: vec![upsert("Note.md", b"hello from mobile\n")],
                ..empty_sync(base.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.status, SyncStatus::Ok);

    let history = service.history(USER, VAULT, Some("Note.md")).await.unwrap();
    assert!(history.len() >= 2);

    let old = service
        .file_at_version(USER, VAULT, "Note.md", base.as_ref().unwrap())
        .await
        .unwrap();
    assert!(old.read_only);
    assert_eq!(
        String::from_utf8(STANDARD.decode(old.content_base64).unwrap()).unwrap(),
        "hello\n"
    );

    let clone = fixture.clone_remote("check").await;
    assert_eq!(
        fs::read_to_string(clone.join("Note.md")).await.unwrap(),
        "hello from mobile\n"
    );
    assert_eq!(registration.user, USER);
}

#[tokio::test]
async fn rebases_remote_changes_and_keeps_history_linear() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    let first = service.sync(USER, VAULT, empty_sync(None)).await.unwrap();

    fixture
        .commit_remote_file("remote-update", "Remote.md", b"remote\n", "remote update")
        .await;

    let synced = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: first.server_head.clone(),
                changes: vec![upsert("Client.md", b"client\n")],
                ..empty_sync(first.server_head.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(synced.status, SyncStatus::Ok);

    let clone = fixture.clone_remote("linear-check").await;
    assert_eq!(
        fs::read_to_string(clone.join("Remote.md")).await.unwrap(),
        "remote\n"
    );
    assert_eq!(
        fs::read_to_string(clone.join("Client.md")).await.unwrap(),
        "client\n"
    );
    let parents = git(Some(&clone), &["log", "--format=%P"], &[0])
        .await
        .unwrap();
    for line in String::from_utf8(parents.stdout).unwrap().lines() {
        assert!(
            line.split_whitespace().count() <= 1,
            "history should not contain merge commits"
        );
    }
}

#[tokio::test]
async fn stores_binary_content_outside_git_and_versions_manifest() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    let first = service.sync(USER, VAULT, empty_sync(None)).await.unwrap();
    let image = vec![0, 1, 2, 3, 255];

    let synced = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: first.server_head.clone(),
                changes: vec![upsert("Images/photo.png", &image)],
                ..empty_sync(first.server_head.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(synced.status, SyncStatus::Ok);

    let repo = fixture
        .root
        .path()
        .join("data/users/alice/vaults/notes/repo");
    let tracked_image = git(Some(&repo), &["ls-files", "Images/photo.png"], &[0])
        .await
        .unwrap();
    assert!(tracked_image.stdout.is_empty());
    let tracked_manifest = git(
        Some(&repo),
        &["ls-files", ".obsidian-git-sync/binary-manifest.json"],
        &[0],
    )
    .await
    .unwrap();
    assert!(!tracked_manifest.stdout.is_empty());

    let version = service
        .file_at_version(
            USER,
            VAULT,
            "Images/photo.png",
            synced.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(STANDARD.decode(version.content_base64).unwrap(), image);

    let second = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: synced.server_head.clone(),
                changes: vec![upsert("Images/other.png", &[9, 8, 7])],
                ..empty_sync(synced.server_head.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(second.status, SyncStatus::Ok);
    let photo_history = service
        .history(USER, VAULT, Some("Images/photo.png"))
        .await
        .unwrap();
    assert_eq!(photo_history.len(), 1);
}

#[tokio::test]
async fn returns_mobile_resolvable_conflict_markers_for_same_file_edits() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    let first = service.sync(USER, VAULT, empty_sync(None)).await.unwrap();
    let base = first.server_head.clone();

    let device_a = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: base.clone(),
                changes: vec![upsert("Note.md", b"hello from A\n")],
                ..empty_sync(base.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(device_a.status, SyncStatus::Ok);

    let device_b = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: base,
                changes: vec![upsert("Note.md", b"hello from B\n")],
                ..empty_sync(first.server_head)
            },
        )
        .await
        .unwrap();
    assert_eq!(device_b.status, SyncStatus::Conflict);
    assert_eq!(device_b.conflicts[0].path, "Note.md");
    let conflict_text = file_text(&device_b.files, "Note.md");
    assert!(conflict_text.contains("<<<<<<< server"));
    assert!(conflict_text.contains(">>>>>>> client"));

    let repo = fixture
        .root
        .path()
        .join("data/users/alice/vaults/notes/repo");
    let status = git(Some(&repo), &["status", "--porcelain"], &[0])
        .await
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "server repo should be clean after returning conflict"
    );

    let blocked = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: device_b.server_head.clone(),
                changes: vec![upsert("Note.md", b"accidental follow-up\n")],
                ..empty_sync(device_b.server_head.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(blocked.status, SyncStatus::Conflict);
    assert_eq!(blocked.conflicts[0].path, "Note.md");
    assert!(
        blocked.files.is_empty(),
        "pending conflict should not overwrite the client's conflict marker file"
    );

    let clone_before_resolve = fixture.clone_remote("blocked-check").await;
    assert_eq!(
        fs::read_to_string(clone_before_resolve.join("Note.md"))
            .await
            .unwrap(),
        "hello from A\n"
    );

    let resolved = service
        .resolve(
            USER,
            VAULT,
            ResolveRequest {
                client_id: "device-b".to_string(),
                device_name: "iPhone".to_string(),
                files: vec![ResolvedFile {
                    path: "Note.md".to_string(),
                    content_base64: Some(STANDARD.encode(b"resolved\n")),
                    upload_id: None,
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(resolved.status, SyncStatus::Ok);
    let clone = fixture.clone_remote("resolved-check").await;
    assert_eq!(
        fs::read_to_string(clone.join("Note.md")).await.unwrap(),
        "resolved\n"
    );
}

#[tokio::test]
async fn brand_new_file_does_not_conflict_when_device_never_touched_the_path() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Existing.md", b"seed\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();

    // A stale device syncs once, before "Ghost.md" is ever created anywhere.
    let stale = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "stale-device".to_string(),
                ..empty_sync(None)
            },
        )
        .await
        .unwrap();
    let stale_base = stale.server_head.clone();

    // A different device creates and then deletes "Ghost.md" without the stale device ever
    // syncing (and therefore never touching that path).
    let creator_base = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "creator-device".to_string(),
                changes: vec![upsert("Ghost.md", b"boo\n")],
                ..empty_sync(stale_base.clone())
            },
        )
        .await
        .unwrap()
        .server_head;
    service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "creator-device".to_string(),
                changes: vec![ClientChange::Delete {
                    path: "Ghost.md".to_string(),
                }],
                ..empty_sync(creator_base)
            },
        )
        .await
        .unwrap();

    // The stale device, still holding its old base_head from before "Ghost.md" ever existed,
    // now creates a brand-new file with that same name. It never saw the create or the delete,
    // so this must not be reported as "server deleted file while client edited it".
    let recreated = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "stale-device".to_string(),
                changes: vec![upsert("Ghost.md", b"totally new content\n")],
                ..empty_sync(stale_base)
            },
        )
        .await
        .unwrap();
    assert_eq!(
        recreated.status,
        SyncStatus::Ok,
        "{:?}",
        recreated.conflicts
    );

    let clone = fixture.clone_remote("brand-new-check").await;
    assert_eq!(
        fs::read_to_string(clone.join("Ghost.md")).await.unwrap(),
        "totally new content\n"
    );
}

#[tokio::test]
async fn stale_edit_of_a_path_the_device_has_seen_before_still_conflicts() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    service.sync(USER, VAULT, empty_sync(None)).await.unwrap();

    // Device B's own first sync picks up Note.md via changed_files_since, so its per-path ack
    // for Note.md is genuinely populated before it goes stale.
    let device_b_first = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "device-b".to_string(),
                ..empty_sync(None)
            },
        )
        .await
        .unwrap();
    let stale_base = device_b_first.server_head.clone();

    // Device A also does its own real first sync (picking up Note.md the same way), then edits it.
    service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "device-a".to_string(),
                ..empty_sync(None)
            },
        )
        .await
        .unwrap();
    let device_a = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "device-a".to_string(),
                base_head: stale_base.clone(),
                changes: vec![upsert("Note.md", b"hello from A\n")],
                ..empty_sync(stale_base.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(device_a.status, SyncStatus::Ok);

    // Device B, still holding its now-stale base_head from before A's edit, edits its own
    // outdated copy of a path it has genuinely seen before.
    let device_b = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                client_id: "device-b".to_string(),
                base_head: stale_base.clone(),
                changes: vec![upsert("Note.md", b"hello from B\n")],
                ..empty_sync(stale_base)
            },
        )
        .await
        .unwrap();
    assert_eq!(
        device_b.status,
        SyncStatus::Conflict,
        "device that has genuinely seen this path before should still get a real conflict"
    );
    assert_eq!(device_b.conflicts[0].path, "Note.md");
}

#[tokio::test]
async fn detects_binary_conflicts_without_overwriting_client_file() {
    let fixture = GitFixture::new().await;
    fixture.seed_file("Note.md", b"hello\n").await;
    let service = VaultService::new_for_tests(fixture.root.path().join("data"));
    service
        .register(USER, VAULT, register_request(&fixture.remote))
        .await
        .unwrap();
    let first = service.sync(USER, VAULT, empty_sync(None)).await.unwrap();
    let base = first.server_head.clone();

    let device_a = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: base.clone(),
                changes: vec![upsert("Images/photo.png", &[1, 1, 1])],
                ..empty_sync(base.clone())
            },
        )
        .await
        .unwrap();
    assert_eq!(device_a.status, SyncStatus::Ok);

    let device_b = service
        .sync(
            USER,
            VAULT,
            SyncRequest {
                base_head: base,
                changes: vec![upsert("Images/photo.png", &[2, 2, 2])],
                ..empty_sync(first.server_head)
            },
        )
        .await
        .unwrap();
    assert_eq!(device_b.status, SyncStatus::Conflict);
    assert_eq!(device_b.conflicts[0].path, "Images/photo.png");
    assert!(
        device_b.files.is_empty(),
        "binary conflict should not overwrite the mobile client's local binary"
    );
}

struct GitFixture {
    root: TempDir,
    remote: PathBuf,
}

impl GitFixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        git(
            Some(root.path()),
            &["init", "--bare", "--initial-branch=main", "remote.git"],
            &[0],
        )
        .await
        .unwrap();
        Self { root, remote }
    }

    async fn seed_file(&self, path: &str, content: &[u8]) {
        let seed = self.root.path().join("seed");
        git(
            Some(self.root.path()),
            &["clone", self.remote.to_str().unwrap(), "seed"],
            &[0],
        )
        .await
        .unwrap();
        git(Some(&seed), &["config", "user.name", "Test User"], &[0])
            .await
            .unwrap();
        git(
            Some(&seed),
            &["config", "user.email", "test@example.invalid"],
            &[0],
        )
        .await
        .unwrap();
        let absolute = seed.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(&absolute, content).await.unwrap();
        git(Some(&seed), &["add", "-A"], &[0]).await.unwrap();
        git(Some(&seed), &["commit", "-m", "seed"], &[0])
            .await
            .unwrap();
        git(Some(&seed), &["push", "-u", "origin", "main"], &[0])
            .await
            .unwrap();
    }

    async fn clone_remote(&self, name: &str) -> PathBuf {
        git(
            Some(self.root.path()),
            &["clone", self.remote.to_str().unwrap(), name],
            &[0],
        )
        .await
        .unwrap();
        self.root.path().join(name)
    }

    async fn commit_remote_file(
        &self,
        clone_name: &str,
        path: &str,
        content: &[u8],
        message: &str,
    ) {
        let clone = self.clone_remote(clone_name).await;
        git(Some(&clone), &["config", "user.name", "Remote User"], &[0])
            .await
            .unwrap();
        git(
            Some(&clone),
            &["config", "user.email", "remote@example.invalid"],
            &[0],
        )
        .await
        .unwrap();
        let absolute = clone.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(absolute, content).await.unwrap();
        git(Some(&clone), &["add", "-A"], &[0]).await.unwrap();
        git(Some(&clone), &["commit", "-m", message], &[0])
            .await
            .unwrap();
        git(Some(&clone), &["push"], &[0]).await.unwrap();
    }
}

fn register_request(remote: &Path) -> RegisterRequest {
    RegisterRequest {
        remote_url: remote.to_string_lossy().to_string(),
        branch: "main".to_string(),
        author_name: "Test User".to_string(),
        author_email: "test@example.invalid".to_string(),
    }
}

fn local_register_request() -> RegisterRequest {
    RegisterRequest {
        remote_url: "".to_string(),
        branch: "main".to_string(),
        author_name: "Test User".to_string(),
        author_email: "test@example.invalid".to_string(),
    }
}

fn empty_sync(base_head: Option<String>) -> SyncRequest {
    SyncRequest {
        base_head,
        client_id: "device".to_string(),
        device_name: "iPhone".to_string(),
        changes: vec![],
        client_manifest: vec![],
    }
}

fn upsert(path: &str, content: &[u8]) -> ClientChange {
    ClientChange::Upsert {
        path: path.to_string(),
        content_base64: Some(STANDARD.encode(content)),
        upload_id: None,
        sha256: None,
        mtime: Some(1),
    }
}

async fn response_text(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn response_json(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn file_text(files: &[obsidian_git_sync_server::protocol::ServerFileChange], path: &str) -> String {
    for file in files {
        if let obsidian_git_sync_server::protocol::ServerFileChange::Upsert {
            path: file_path,
            content_base64,
            ..
        } = file
        {
            if file_path == path {
                return String::from_utf8(STANDARD.decode(content_base64).unwrap()).unwrap();
            }
        }
    }
    panic!("file not found in response: {path}");
}
