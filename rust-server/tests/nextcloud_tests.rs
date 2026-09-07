//! The Nextcloud surface the Saber app uses: login flow v2, OCS user lookup, WebDAV under
//! `/remote.php/webdav/Saber/`, and server-side rendering of encrypted notes to PDFs.

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, Response, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bson::spec::BinarySubtype;
use bson::{doc, Binary, Bson};
use obsidian_git_sync_server::auth::AuthVerifier;
use obsidian_git_sync_server::device_passwords::CreateSaberDevice;
use obsidian_git_sync_server::http::{router, AppState, PublicAuthConfig};
use obsidian_git_sync_server::protocol::RegisterRequest;
use obsidian_git_sync_server::saber::crypto::SaberCipher;
use obsidian_git_sync_server::vault::VaultService;
use serde_json::Value;
use std::time::Duration;
use tower::ServiceExt;

const BEARER: &str = "Bearer secret";
const ENC_PASSWORD: &str = "correct horse";
const IV: [u8; 16] = [9; 16];

fn state(root: &std::path::Path) -> AppState {
    AppState::new(
        VaultService::new(root.join("data")),
        AuthVerifier::StaticTokenForDev {
            token: "secret".to_string(),
            user: "alice".to_string(),
        },
        PublicAuthConfig::Token,
    )
    .with_saber_render_delay(Duration::ZERO)
}

fn app(state: &AppState) -> axum::Router {
    router(state.clone(), 1024 * 1024, Vec::new())
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

async fn saber_password(state: &AppState, encryption_password: &str) -> String {
    state
        .device_passwords
        .create_saber(
            "alice",
            "notes",
            CreateSaberDevice {
                label: "Saber on iPad".to_string(),
                folder: "Saber/Sync".to_string(),
                pdf_folder: "Saber".to_string(),
                encryption_password: encryption_password.to_string(),
            },
        )
        .await
        .unwrap()
        .password
}

fn basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    )
}

async fn request(
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
    String::from_utf8_lossy(&bytes).to_string()
}

async fn json(response: Response<Body>) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn cipher() -> SaberCipher {
    SaberCipher::new(ENC_PASSWORD, &STANDARD.encode(IV)).unwrap()
}

fn config_sbc(cipher: &SaberCipher) -> Vec<u8> {
    let key_check = STANDARD.encode(cipher.encrypt(STANDARD.encode([3_u8; 32]).as_bytes()));
    serde_json::to_vec(&serde_json::json!({ "iv": STANDARD.encode(IV), "key": key_check })).unwrap()
}

fn point(x: f32, y: f32) -> Bson {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&0.5_f32.to_le_bytes());
    Bson::Binary(Binary {
        subtype: BinarySubtype::Generic,
        bytes,
    })
}

fn sample_note_bytes() -> Vec<u8> {
    let document = doc! {
        "v": 19_i32,
        "ni": 1_i32,
        "p": "college",
        "l": 40_i32,
        "lt": 3_i32,
        "z": [
            {
                "w": 1000.0,
                "h": 1400.0,
                "s": [
                    {
                        "shape": Bson::Null,
                        "p": [point(100.0, 100.0), point(220.0, 140.0), point(400.0, 90.0), point(500.0, 200.0)],
                        "i": 0_i32,
                        "ty": "fountainPen",
                        "pe": true,
                        "c": 0xff00_0000_i64
                    }
                ],
                "i": [
                    { "id": 0_i32, "e": ".png", "i": 0_i32, "x": 600.0, "y": 600.0, "w": 200.0, "h": 100.0, "a": 0_i32 }
                ],
                "q": [ { "insert": "Typed heading\n" } ]
            },
            { "w": 1000.0, "h": 1400.0 }
        ],
        "c": 0_i32
    };
    let mut bytes = Vec::new();
    document.to_writer(&mut bytes).unwrap();
    bytes
}

fn sample_png() -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 2, image::Rgb([200, 30, 30])))
        .write_to(&mut buffer, image::ImageFormat::Png)
        .unwrap();
    buffer.into_inner()
}

async fn put_saber_file(app: &axum::Router, auth: &str, name: &str, body: Vec<u8>) -> StatusCode {
    request(
        app,
        "PUT",
        &format!("/remote.php/webdav/Saber/{name}"),
        Some(auth),
        &[("x-oc-mtime", "1700000000")],
        body,
    )
    .await
    .status()
}

#[tokio::test]
async fn status_and_capabilities_look_like_nextcloud() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&state(dir.path()));
    let response = request(&app, "GET", "/status.php", None, &[], vec![]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["installed"], true);
    assert_eq!(body["productname"], "ObsidiSync");

    let response = request(
        &app,
        "GET",
        "/ocs/v2.php/cloud/capabilities",
        None,
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["ocs"]["meta"]["status"], "ok");
    assert_eq!(
        body["ocs"]["data"]["capabilities"]["core"]["webdav-root"],
        "remote.php/webdav"
    );
}

#[tokio::test]
async fn login_flow_issues_a_saber_device_password() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    register(&app).await;

    let response = request(
        &app,
        "POST",
        "/index.php/login/v2",
        None,
        &[
            ("host", "sync.example.test"),
            ("x-forwarded-proto", "https"),
        ],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let init = json(response).await;
    let login_url = init["login"].as_str().unwrap().to_string();
    let poll_token = init["poll"]["token"].as_str().unwrap().to_string();
    assert_eq!(
        init["poll"]["endpoint"],
        "https://sync.example.test/index.php/login/v2/poll"
    );
    assert!(login_url.starts_with("https://sync.example.test/index.php/login/v2/flow/"));
    let flow_path = login_url
        .trim_start_matches("https://sync.example.test")
        .to_string();

    // Not completed yet: the app keeps polling.
    let response = request(
        &app,
        "POST",
        "/index.php/login/v2/poll",
        None,
        &[("content-type", "application/json")],
        serde_json::json!({ "token": poll_token })
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Token mode has no browser session: the page asks for an access token.
    let response = request(&app, "GET", &flow_path, None, &[], vec![]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("Connect Saber"));
    assert!(page.contains("name=\"access_token\""));
    assert!(page.contains("name=\"vault\""));

    let form = serde_urlencoded::to_string([
        ("access_token", "secret"),
        ("vault", "notes"),
        ("label", "Saber on iPad"),
        ("folder", "Saber/Sync"),
        ("pdf_folder", "Saber"),
        ("encryption_password", ENC_PASSWORD),
    ])
    .unwrap();
    let response = request(
        &app,
        "POST",
        &flow_path,
        None,
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("host", "sync.example.test"),
            ("x-forwarded-proto", "https"),
        ],
        form.into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let done = text(response).await;
    assert!(
        done.contains("nc://login/server:https://sync.example.test&amp;user:alice&amp;password:")
    );

    let response = request(
        &app,
        "POST",
        "/index.php/login/v2/poll",
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        format!("token={poll_token}").into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let credentials = json(response).await;
    assert_eq!(credentials["loginName"], "alice");
    let app_password = credentials["appPassword"].as_str().unwrap().to_string();
    assert_eq!(app_password.len(), 24);

    // A second poll finds nothing: credentials are handed out once.
    let response = request(
        &app,
        "POST",
        "/index.php/login/v2/poll",
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        format!("token={poll_token}").into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let entries = state.device_passwords.list("alice", "notes").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "Saber on iPad");
    assert_eq!(entries[0].pdf_folder.as_deref(), Some("Saber"));
    assert_eq!(serde_json::to_value(&entries[0]).unwrap()["kind"], "saber");

    // The password works for the OCS user lookup Saber performs right after login.
    let auth = basic("alice", &app_password);
    let response = request(
        &app,
        "GET",
        "/ocs/v2.php/cloud/user",
        Some(&auth),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let user = json(response).await;
    assert_eq!(user["ocs"]["data"]["id"], "alice");
    let response = request(
        &app,
        "GET",
        "/ocs/v2.php/cloud/user",
        Some(&basic("alice", "wrong")),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = request(
        &app,
        "GET",
        "/index.php/avatar/alice/512",
        None,
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
}

#[tokio::test]
async fn login_flow_rejects_unknown_vault_and_expired_links() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    let init = json(
        request(
            &app,
            "POST",
            "/index.php/login/v2",
            None,
            &[("host", "h")],
            vec![],
        )
        .await,
    )
    .await;
    let flow_path = init["login"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://h")
        .to_string();
    let form =
        serde_urlencoded::to_string([("access_token", "secret"), ("vault", "nope")]).unwrap();
    let response = request(
        &app,
        "POST",
        &flow_path,
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        form.into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .contains("error=Unknown%20vault"));

    let response = request(
        &app,
        "GET",
        "/index.php/login/v2/flow/doesnotexist",
        None,
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webdav_exposes_the_granted_folder_as_saber() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    register(&app).await;
    let password = saber_password(&state, "").await;
    let auth = basic("alice", &password);

    let response = request(
        &app,
        "PROPFIND",
        "/remote.php/webdav/",
        Some(&auth),
        &[("depth", "1")],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::MULTI_STATUS);
    let body = text(response).await;
    assert!(body.contains("<D:href>/remote.php/webdav/Saber/</D:href>"));
    assert!(!body.contains("notes"), "vault name must not leak: {body}");

    // Saber creates the folder first; a second MKCOL says it already exists.
    let response = request(
        &app,
        "MKCOL",
        "/remote.php/webdav/Saber",
        Some(&auth),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let response = request(
        &app,
        "PUT",
        "/remote.php/webdav/Saber/config.sbc",
        Some(&auth),
        &[("x-oc-mtime", "1700000000")],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["x-oc-mtime"], "accepted");

    let response = request(
        &app,
        "PROPFIND",
        "/remote.php/webdav/Saber/",
        Some(&auth),
        &[("depth", "1")],
        vec![],
    )
    .await;
    let body = text(response).await;
    assert!(body.contains("<D:href>/remote.php/webdav/Saber/config.sbc</D:href>"));
    assert!(
        body.contains("<D:getlastmodified>Tue, 14 Nov 2023 22:13:20 GMT</D:getlastmodified>"),
        "{body}"
    );

    // The same tree is reachable through the files DAV endpoint.
    let response = request(
        &app,
        "GET",
        "/remote.php/dav/files/alice/Saber/config.sbc",
        Some(&auth),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, "{}");

    let response = request(
        &app,
        "PROPFIND",
        "/remote.php/webdav/Other/",
        Some(&auth),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = request(&app, "PROPFIND", "/remote.php/webdav/", None, &[], vec![]).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The file landed in the vault at the granted folder.
    let entry = state
        .vaults
        .dav_stat("alice", "notes", "Saber/Sync/config.sbc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.mtime_millis, 1_700_000_000_000);
}

#[tokio::test]
async fn encrypted_notes_become_pdfs_and_deletions_remove_them() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    register(&app).await;
    let password = saber_password(&state, ENC_PASSWORD).await;
    let auth = basic("alice", &password);
    let cipher = cipher();

    assert_eq!(
        put_saber_file(&app, &auth, "config.sbc", config_sbc(&cipher)).await,
        StatusCode::CREATED
    );
    let note_name = cipher.encrypt_file_name("/Uni/Lecture 1.sbn2");
    let asset_name = cipher.encrypt_file_name("/Uni/Lecture 1.sbn2.0");
    let preview_name = cipher.encrypt_file_name("/Uni/Lecture 1.sbn2.p");
    assert_eq!(
        put_saber_file(&app, &auth, &preview_name, cipher.encrypt(&sample_png())).await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_saber_file(&app, &auth, &asset_name, cipher.encrypt(&sample_png())).await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_saber_file(
            &app,
            &auth,
            &note_name,
            cipher.encrypt(&sample_note_bytes())
        )
        .await,
        StatusCode::CREATED
    );
    state.saber.wait_idle().await;

    let (entry, pdf) = state
        .vaults
        .dav_read("alice", "notes", "Saber/Uni/Lecture 1.pdf")
        .await
        .expect("PDF rendered into the vault");
    assert!(
        pdf.starts_with(b"%PDF-1."),
        "not a PDF: {:?}",
        &pdf[..8.min(pdf.len())]
    );
    assert!(entry.size > 500);
    let pdf_text = String::from_utf8_lossy(&pdf);
    assert!(
        pdf_text.contains("/Count 1"),
        "empty trailing page should be dropped"
    );
    assert!(
        pdf_text.contains("/Subtype /Image"),
        "the PNG asset should be embedded"
    );

    // The encrypted originals stay in the sync folder for Saber's own multi-device sync.
    let listing = state
        .vaults
        .dav_list("alice", "notes", "Saber/Sync")
        .await
        .unwrap();
    assert_eq!(listing.len(), 4, "{listing:?}");

    // History records the PDF as written by the Saber device.
    let response = request(
        &app,
        "GET",
        "/v1/users/alice/vaults/notes/history?path=Saber/Uni/Lecture%201.pdf",
        Some(BEARER),
        &[],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let history = text(response).await;
    assert!(history.contains("Saber on iPad"), "{history}");

    // Re-uploading identical bytes changes nothing and does not fail.
    assert_eq!(
        put_saber_file(
            &app,
            &auth,
            &note_name,
            cipher.encrypt(&sample_note_bytes())
        )
        .await,
        StatusCode::NO_CONTENT
    );
    state.saber.wait_idle().await;
    let (entry_again, _) = state
        .vaults
        .dav_read("alice", "notes", "Saber/Uni/Lecture 1.pdf")
        .await
        .unwrap();
    assert_eq!(entry_again.etag, entry.etag);

    // Saber signals deletion by uploading an empty file.
    assert_eq!(
        put_saber_file(&app, &auth, &note_name, Vec::new()).await,
        StatusCode::NO_CONTENT
    );
    state.saber.wait_idle().await;
    assert!(state
        .vaults
        .dav_stat("alice", "notes", "Saber/Uni/Lecture 1.pdf")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn wrong_encryption_password_stores_files_without_rendering() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    register(&app).await;
    let password = saber_password(&state, "not the password").await;
    let auth = basic("alice", &password);
    let cipher = cipher();

    assert_eq!(
        put_saber_file(&app, &auth, "config.sbc", config_sbc(&cipher)).await,
        StatusCode::CREATED
    );
    let note_name = cipher.encrypt_file_name("/Note.sbn2");
    assert_eq!(
        put_saber_file(
            &app,
            &auth,
            &note_name,
            cipher.encrypt(&sample_note_bytes())
        )
        .await,
        StatusCode::CREATED
    );
    state.saber.wait_idle().await;

    let listing = state
        .vaults
        .dav_list("alice", "notes", "Saber/Sync")
        .await
        .unwrap();
    assert_eq!(listing.len(), 2);
    assert!(state
        .vaults
        .dav_stat("alice", "notes", "Saber/Note.pdf")
        .await
        .unwrap()
        .is_none());

    let grant = state
        .device_passwords
        .authenticate("alice", &password)
        .await
        .unwrap();
    let error = state
        .saber
        .render_paths(&grant, &[format!("Saber/Sync/{note_name}")])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("encryption password"),
        "{error:#}"
    );
}

#[tokio::test]
async fn saber_pdf_folder_must_not_overlap_sync_folder() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = app(&state);
    register(&app).await;
    let error = state
        .device_passwords
        .create_saber(
            "alice",
            "notes",
            CreateSaberDevice {
                label: "Saber".to_string(),
                folder: "Saber".to_string(),
                pdf_folder: "Saber/PDF".to_string(),
                encryption_password: String::new(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("separate"), "{error}");
}

#[tokio::test]
async fn password_mode_sends_the_browser_through_login_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let state = AppState::new(
        VaultService::new(data_dir.clone()),
        AuthVerifier::password("alice".to_string(), data_dir).unwrap(),
        PublicAuthConfig::Password,
    );
    let app = app(&state);

    // Set the password and register a vault with the returned token.
    let response = request(
        &app,
        "POST",
        "/login",
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        b"username=alice&password=correct-horse-battery-staple&password_confirm=correct-horse-battery-staple".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let setup_html = text(response).await;
    let token_start = setup_html.find("<textarea readonly>").unwrap() + "<textarea readonly>".len();
    let token_end = setup_html[token_start..].find("</textarea>").unwrap() + token_start;
    let bearer = format!("Bearer {}", &setup_html[token_start..token_end]);
    let response = request(
        &app,
        "POST",
        "/v1/users/alice/vaults/notes/register",
        Some(&bearer),
        &[("content-type", "application/json")],
        serde_json::to_vec(&RegisterRequest {
            remote_url: String::new(),
            branch: "main".to_string(),
            author_name: "Test".to_string(),
            author_email: "test@example.invalid".to_string(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let init = json(
        request(
            &app,
            "POST",
            "/index.php/login/v2",
            None,
            &[("host", "h")],
            vec![],
        )
        .await,
    )
    .await;
    let flow_path = init["login"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://h")
        .to_string();

    // Without a browser session the flow page bounces to the login page and back.
    let response = request(&app, "GET", &flow_path, None, &[], vec![]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.starts_with("/login?next=%2Findex.php%2Flogin%2Fv2%2Fflow%2F"),
        "{location}"
    );

    let response = request(&app, "GET", &location, None, &[], vec![]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let login_html = text(response).await;
    assert!(
        login_html.contains(&format!(
            r#"<input type="hidden" name="next" value="{flow_path}">"#
        )),
        "{login_html}"
    );

    let form = serde_urlencoded::to_string([
        ("username", "alice"),
        ("password", "correct-horse-battery-staple"),
        ("next", flow_path.as_str()),
    ])
    .unwrap();
    let response = request(
        &app,
        "POST",
        "/login",
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        form.into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], flow_path.as_str());
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Absolute URLs are never followed after login.
    let form = serde_urlencoded::to_string([
        ("username", "alice"),
        ("password", "correct-horse-battery-staple"),
        ("next", "https://evil.example/phish"),
    ])
    .unwrap();
    let response = request(
        &app,
        "POST",
        "/login",
        None,
        &[("content-type", "application/x-www-form-urlencoded")],
        form.into_bytes(),
    )
    .await;
    assert_eq!(response.headers()[header::LOCATION], "/change-feed");

    let response = request(
        &app,
        "GET",
        &flow_path,
        None,
        &[("cookie", cookie.as_str())],
        vec![],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = text(response).await;
    assert!(
        page.contains("Signed in as <strong>alice</strong>"),
        "{page}"
    );
    assert!(page.contains("<option value=\"notes\">"), "{page}");
    assert!(!page.contains("name=\"access_token\""));

    let form = serde_urlencoded::to_string([("vault", "notes"), ("label", "Saber")]).unwrap();
    let response = request(
        &app,
        "POST",
        &flow_path,
        None,
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("cookie", cookie.as_str()),
            ("host", "h"),
        ],
        form.into_bytes(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(response).await.contains("Saber is connected"));
    let entries = state.device_passwords.list("alice", "notes").await.unwrap();
    assert_eq!(entries[0].folder, "Saber/Sync");
    assert_eq!(entries[0].pdf_folder.as_deref(), Some("Saber"));
}
