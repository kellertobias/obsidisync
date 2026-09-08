use base64::{engine::general_purpose::STANDARD, Engine};
use obsidian_git_sync_server::{binary_store::sha256_hex, protocol::*, vault::VaultService};
use serde_json::{json, Value};
const ROOT: &str = ".inkvault/notes/11111111-1111-4111-8111-111111111111";
const PAGE: &str = "22222222-2222-4222-8222-222222222222";
fn cbor(v: &Value) -> Vec<u8> {
    fn head(t: u8, n: u64) -> Vec<u8> {
        if n < 24 {
            vec![t * 32 + n as u8]
        } else if n <= 255 {
            vec![t * 32 + 24, n as u8]
        } else if n <= 65535 {
            [vec![t * 32 + 25], (n as u16).to_be_bytes().to_vec()].concat()
        } else {
            [vec![t * 32 + 26], (n as u32).to_be_bytes().to_vec()].concat()
        }
    }
    match v {
        Value::Null => vec![246],
        Value::Bool(b) => vec![if *b { 245 } else { 244 }],
        Value::Number(n) => head(0, n.as_u64().unwrap()),
        Value::String(s) => [head(3, s.len() as u64), s.as_bytes().to_vec()].concat(),
        Value::Array(a) => [head(4, a.len() as u64), a.iter().flat_map(cbor).collect()].concat(),
        Value::Object(o) => {
            let mut fields: Vec<_> = o.iter().map(|(k, v)| (cbor(&json!(k)), cbor(v))).collect();
            fields.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then(a.0.cmp(&b.0)));
            [
                head(5, o.len() as u64),
                fields
                    .into_iter()
                    .flat_map(|(k, v)| [k, v].concat())
                    .collect(),
            ]
            .concat()
        }
    }
}
fn upsert(path: String, bytes: Vec<u8>) -> ClientChange {
    ClientChange::Upsert {
        path,
        sha256: Some(sha256_hex(&bytes)),
        content_base64: Some(STANDARD.encode(bytes)),
        upload_id: None,
        mtime: Some(1),
    }
}
fn changes(rev: i64) -> Vec<ClientChange> {
    let page = cbor(
        &json!({"schemaVersion":1,"width":210000,"height":297000,"strokes":[{"id":"s1","points":[[10000,10000,1,1000,null],[90000,90000,2,1000,null]],"style":{"tool":"pen","color":4278190080u64,"width":500,"pressure":true}}],"objects":[],"tombstones":[]}),
    );
    let manifest = json!({"schemaVersion":1,"documentId":ROOT.rsplit('/').next().unwrap(),"pdfPath":"Notes/Test.pdf","title":"Test","created":1,"modified":rev,"sourceRevision":rev,"renderRevision":"","pages":[{"id":PAGE,"width":210000,"height":297000,"orientation":"portrait","template":null,"sha256":sha256_hex(&page)}]});
    vec![
        upsert(
            format!("{ROOT}/manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        upsert(format!("{ROOT}/pages/{PAGE}.cbor"), page),
    ]
}
fn request(head: Option<String>, changes: Vec<ClientChange>) -> SyncRequest {
    SyncRequest {
        base_head: head,
        client_id: "inkvault-test".into(),
        device_name: "InkVault test".into(),
        changes,
        client_manifest: vec![],
        file_content: FileContentMode::Reference,
    }
}
async fn setup() -> (tempfile::TempDir, VaultService, Option<String>) {
    let dir = tempfile::tempdir().unwrap();
    let service = VaultService::new_for_tests(dir.path().into());
    let r = service
        .register(
            "alice",
            "notes",
            RegisterRequest {
                remote_url: "".into(),
                branch: "main".into(),
                author_name: "Tester".into(),
                author_email: "test@example.com".into(),
            },
        )
        .await
        .unwrap();
    (dir, service, r.server_head)
}
#[tokio::test]
async fn publication_is_paired_replayable_and_conflict_safe() {
    let (_dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial.clone(), changes(1)))
        .await
        .unwrap();
    assert_eq!(first.status, SyncStatus::Ok);
    assert_eq!(first.files.len(), 3);
    let pdf = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            first.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    assert_eq!(
        lopdf::Document::load_mem(&pdf).unwrap().get_pages().len(),
        1
    );
    let replay = service
        .sync_inkvault("alice", "notes", request(initial.clone(), changes(1)))
        .await
        .unwrap();
    assert_eq!(replay.server_head, first.server_head);
    let second = service
        .sync_inkvault(
            "alice",
            "notes",
            request(first.server_head.clone(), changes(2)),
        )
        .await
        .unwrap();
    assert_ne!(second.server_head, first.server_head);
    let conflict = service
        .sync_inkvault("alice", "notes", request(first.server_head, changes(3)))
        .await
        .unwrap();
    assert_eq!(conflict.status, SyncStatus::Conflict);
    assert_eq!(conflict.conflicts.len(), 3);
    assert_eq!(conflict.server_head, second.server_head);
    // An invalid new page cannot replace either half of the committed note.
    let mut invalid = changes(4);
    invalid[1] = upsert(format!("{ROOT}/pages/{PAGE}.cbor"), vec![255]);
    assert!(service
        .sync_inkvault(
            "alice",
            "notes",
            request(second.server_head.clone(), invalid)
        )
        .await
        .is_err());
    let after = service
        .sync_inkvault("alice", "notes", request(None, vec![]))
        .await
        .unwrap();
    assert_eq!(after.server_head, second.server_head);
    assert!(service
        .sync(
            "alice",
            "notes",
            request(
                second.server_head.clone(),
                vec![upsert("Notes/Test.pdf".into(), b"bad".to_vec())]
            )
        )
        .await
        .is_err());
    // Fixed source inputs render exactly the same bytes.
    let pdf2 = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            second.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    assert_eq!(pdf, pdf2);
}
#[tokio::test]
async fn incomplete_upload_never_publishes_and_can_resume() {
    let (_dir, service, head) = setup().await;
    let mut changes = changes(1);
    let (path, bytes, sha) = match &changes[1] {
        ClientChange::Upsert {
            path,
            content_base64,
            sha256,
            ..
        } => (
            path.clone(),
            STANDARD.decode(content_base64.as_ref().unwrap()).unwrap(),
            sha256.clone().unwrap(),
        ),
        _ => unreachable!(),
    };
    let upload = service
        .init_upload(
            "alice",
            "notes",
            UploadInitRequest {
                path: path.clone(),
                sha256: sha.clone(),
                size: bytes.len() as u64,
            },
        )
        .await
        .unwrap();
    changes[1] = ClientChange::Upsert {
        path,
        content_base64: None,
        upload_id: Some(upload.upload_id.clone()),
        sha256: Some(sha),
        mtime: Some(1),
    };
    assert!(service
        .sync_inkvault("alice", "notes", request(head.clone(), changes.clone()))
        .await
        .is_err());
    assert_eq!(
        service
            .sync_inkvault("alice", "notes", request(None, vec![]))
            .await
            .unwrap()
            .server_head,
        head
    );
    service
        .append_upload_chunk(
            "alice",
            "notes",
            &upload.upload_id,
            UploadChunkRequest {
                offset: 0,
                content_base64: STANDARD.encode(&bytes),
            },
        )
        .await
        .unwrap();
    service
        .complete_upload("alice", "notes", &upload.upload_id)
        .await
        .unwrap();
    assert_eq!(
        service
            .sync_inkvault("alice", "notes", request(head, changes))
            .await
            .unwrap()
            .status,
        SyncStatus::Ok
    );
}

#[tokio::test]
async fn recovers_publication_before_and_after_git_ref_update() {
    use obsidian_git_sync_server::{
        binary_store::{read_manifest, BINARY_MANIFEST_PATH},
        git::git,
    };
    let (dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial, changes(1)))
        .await
        .unwrap();
    let second = service
        .sync_inkvault(
            "alice",
            "notes",
            request(first.server_head.clone(), changes(2)),
        )
        .await
        .unwrap();
    let vault = dir.path().join("users/alice/vaults/notes");
    let repo = vault.join("repo");
    let ledger = read_manifest(&repo).await.unwrap();
    let journal =
        json!({"old_head":first.server_head,"new_head":second.server_head,"manifest":ledger});
    for moved in [false, true] {
        git(
            Some(&repo),
            &["reset", "--hard", first.server_head.as_ref().unwrap()],
            &[0],
        )
        .await
        .unwrap();
        if moved {
            git(
                Some(&repo),
                &["update-ref", "HEAD", second.server_head.as_ref().unwrap()],
                &[0],
            )
            .await
            .unwrap();
        }
        std::fs::write(
            vault.join("inkvault-publication.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        let restarted = VaultService::new_for_tests(dir.path().into());
        let response = restarted
            .sync_inkvault("alice", "notes", request(None, vec![]))
            .await
            .unwrap();
        assert_eq!(response.server_head, second.server_head);
        assert_eq!(read_manifest(&repo).await.unwrap(), ledger);
        assert!(!vault.join("inkvault-publication.json").exists());
        assert_eq!(
            git(
                Some(&repo),
                &["diff", "--name-only", "HEAD", "--", BINARY_MANIFEST_PATH],
                &[0]
            )
            .await
            .unwrap()
            .stdout,
            b""
        );
    }
}

#[tokio::test]
async fn native_visibility_dav_guards_and_compare_and_swap_resolution() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use obsidian_git_sync_server::{
        auth::AuthVerifier,
        http::{router, AppState, PublicAuthConfig},
        vault::dav::DavDevice,
    };
    use tower::ServiceExt;
    let (_dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial, changes(1)))
        .await
        .unwrap();
    let listed = service.dav_list("alice", "notes", "").await.unwrap();
    assert!(listed.iter().all(|e| !e.path.starts_with('.')));
    assert!(service.dav_stat("alice", "notes", ROOT).await.is_err());
    assert!(service
        .dav_delete(
            "alice",
            "notes",
            "Notes",
            &DavDevice {
                client_id: "dav".into(),
                name: "DAV".into()
            }
        )
        .await
        .is_err());
    let second = service
        .sync_inkvault(
            "alice",
            "notes",
            request(first.server_head.clone(), changes(2)),
        )
        .await
        .unwrap();
    assert!(service
        .resolve_inkvault("alice", "notes", request(first.server_head, changes(3)))
        .await
        .is_err());
    assert_eq!(
        service
            .resolve_inkvault("alice", "notes", request(second.server_head, changes(3)))
            .await
            .unwrap()
            .status,
        SyncStatus::Ok
    );
    let app = router(
        AppState::new(
            service,
            AuthVerifier::StaticTokenForDev {
                token: "secret".into(),
                user: "alice".into(),
            },
            PublicAuthConfig::Token,
        ),
        1024 * 1024,
        vec![],
    );
    for native in [false, true] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/users/alice/vaults/notes/sync")
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json");
        if native {
            req = req.header("x-obsidisync-client-features", "inkVaultNotesV1");
        }
        let response = app
            .clone()
            .oneshot(
                req.body(Body::from(
                    serde_json::to_vec(&request(None, vec![])).unwrap(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), if native { 3 } else { 1 });
        assert!(files.iter().any(|f| f["path"] == "Notes/Test.pdf"));
    }
}

#[tokio::test]
async fn annotation_preserves_original_pdf_and_pair_deletion_is_atomic() {
    let (_dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial, changes(1)))
        .await
        .unwrap();
    let pdf = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            first.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    // Import an ordinary PDF, then annotate that exact base revision.
    let imported = service
        .sync(
            "alice",
            "notes",
            request(
                first.server_head,
                vec![upsert("Imported.pdf".into(), pdf.clone())],
            ),
        )
        .await
        .unwrap();
    let new_root = ".inkvault/notes/33333333-3333-4333-8333-333333333333";
    let mut annotation = changes(1);
    for change in &mut annotation {
        if let ClientChange::Upsert {
            path,
            content_base64,
            sha256,
            ..
        } = change
        {
            *path = path.replace(ROOT, new_root);
            if path.ends_with("manifest.json") {
                let mut m: Value = serde_json::from_slice(
                    &STANDARD.decode(content_base64.as_ref().unwrap()).unwrap(),
                )
                .unwrap();
                m["documentId"] = json!(new_root.rsplit('/').next().unwrap());
                m["pdfPath"] = json!("Imported.pdf");
                m["basePdfHash"] = json!(sha256_hex(&pdf));
                m["basePdfRevision"] = Value::Null;
                let bytes = serde_json::to_vec(&m).unwrap();
                *sha256 = Some(sha256_hex(&bytes));
                *content_base64 = Some(STANDARD.encode(bytes));
            }
        }
    }
    let result = service
        .sync_inkvault(
            "alice",
            "notes",
            request(imported.server_head.clone(), annotation.clone()),
        )
        .await
        .unwrap();
    let replay = service
        .sync_inkvault(
            "alice",
            "notes",
            request(imported.server_head.clone(), annotation),
        )
        .await
        .unwrap();
    assert_eq!(replay.server_head, result.server_head);
    let saved_source = service
        .file_bytes_at_version(
            "alice",
            "notes",
            &format!("{new_root}/manifest.json"),
            result.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    assert_eq!(
        serde_json::from_slice::<Value>(&saved_source).unwrap()["basePdfRevision"],
        json!(imported.server_head)
    );
    let annotated = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Imported.pdf",
            result.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    let doc = lopdf::Document::load_mem(&annotated).unwrap();
    let content = doc.get_page_content(*doc.get_pages().values().next().unwrap());
    assert!(
        String::from_utf8_lossy(&content)
            .matches("90000 90000 l S")
            .count()
            >= 2
    );
    let deletion = vec![
        ClientChange::Delete {
            path: format!("{new_root}/manifest.json"),
        },
        ClientChange::Delete {
            path: format!("{new_root}/pages/{PAGE}.cbor"),
        },
    ];
    let deleted = service
        .sync_inkvault(
            "alice",
            "notes",
            request(result.server_head.clone(), deletion.clone()),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status, SyncStatus::Ok);
    let replay = service
        .sync_inkvault(
            "alice",
            "notes",
            request(result.server_head.clone(), deletion),
        )
        .await
        .unwrap();
    assert_eq!(replay.server_head, deleted.server_head);

    assert!(service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Imported.pdf",
            deleted.server_head.as_ref().unwrap()
        )
        .await
        .is_err());
    assert!(service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Imported.pdf",
            result.server_head.as_ref().unwrap()
        )
        .await
        .is_ok());
}

#[test]
fn renders_png_svg_and_vector_pdf_templates_without_flattening_pdf_content() {
    use obsidian_git_sync_server::inkvault;
    use std::collections::BTreeMap;
    let package = changes(1);
    let mut files: BTreeMap<String, Vec<u8>> = package
        .iter()
        .map(|c| match c {
            ClientChange::Upsert {
                path,
                content_base64,
                ..
            } => (
                path.clone(),
                STANDARD.decode(content_base64.as_ref().unwrap()).unwrap(),
            ),
            _ => unreachable!(),
        })
        .collect();
    let mut manifest: Value =
        serde_json::from_slice(&files[&format!("{ROOT}/manifest.json")]).unwrap();
    let base = inkvault::render(&manifest, &files, None).unwrap();
    let template = format!("{ROOT}/assets/{}.pdf", sha256_hex(&base));
    files.insert(template.clone(), base);
    manifest["pages"][0]["template"] = json!({"asset":template,"page":0});
    let svg=b"<svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'><rect width='20' height='20' fill='red'/></svg>".to_vec();
    let svg_path = format!("{ROOT}/assets/{}.svg", sha256_hex(&svg));
    files.insert(svg_path.clone(), svg);
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(20, 20)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png_path = format!("{ROOT}/assets/{}.png", sha256_hex(png.get_ref()));
    files.insert(png_path.clone(), png.into_inner());
    let page = cbor(
        &json!({"schemaVersion":1,"width":210000,"height":297000,"strokes":[],"tombstones":[],"objects":[{"id":"a","asset":svg_path,"x":1000,"y":1000,"width":20000,"height":20000},{"id":"b","asset":png_path,"x":30000,"y":1000,"width":20000,"height":20000}]}),
    );
    manifest["pages"][0]["sha256"] = json!(sha256_hex(&page));
    files.insert(format!("{ROOT}/pages/{PAGE}.cbor"), page);
    let rendered = inkvault::render(&manifest, &files, None).unwrap();
    let doc = lopdf::Document::load_mem(&rendered).unwrap();
    assert!(doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .any(
            |s| s.dict.get(b"Subtype").and_then(lopdf::Object::as_name).ok() == Some(b"Form")
                && String::from_utf8_lossy(&s.content).contains("90000 90000 l S")
        ));
    assert_eq!(rendered, inkvault::render(&manifest, &files, None).unwrap());
}

#[tokio::test]
async fn publishes_500_pages_with_mixed_a4_orientations() {
    let (_dir, service, head) = setup().await;
    let mut changes = changes(1);
    let mut manifest: Value = match &changes[0] {
        ClientChange::Upsert { content_base64, .. } => {
            serde_json::from_slice(&STANDARD.decode(content_base64.as_ref().unwrap()).unwrap())
                .unwrap()
        }
        _ => unreachable!(),
    };
    changes.clear();
    let mut pages = Vec::new();
    for i in 0..500 {
        let id = format!("22222222-2222-4222-8222-{i:012}");
        let (w, h) = if i % 2 == 0 {
            (210000, 297000)
        } else {
            (297000, 210000)
        };
        let bytes = cbor(
            &json!({"schemaVersion":1,"width":w,"height":h,"strokes":[],"objects":[],"tombstones":[]}),
        );
        pages.push(json!({"id":id,"width":w,"height":h,"orientation":if w>h{"landscape"}else{"portrait"},"template":null,"sha256":sha256_hex(&bytes)}));
        changes.push(upsert(format!("{ROOT}/pages/{id}.cbor"), bytes));
    }
    manifest["pages"] = json!(pages);
    changes.push(upsert(
        format!("{ROOT}/manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    ));
    let result = service
        .sync_inkvault("alice", "notes", request(head, changes))
        .await
        .unwrap();
    let pdf = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            result.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    assert_eq!(
        lopdf::Document::load_mem(&pdf).unwrap().get_pages().len(),
        500
    );
}

#[tokio::test]
async fn rename_publishes_source_and_pdf_together_and_rejects_collisions() {
    let (_dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial, changes(1)))
        .await
        .unwrap();
    let mut renamed = changes(2);
    if let ClientChange::Upsert { content_base64, .. } = &renamed[0] {
        let mut manifest: Value =
            serde_json::from_slice(&STANDARD.decode(content_base64.as_ref().unwrap()).unwrap())
                .unwrap();
        manifest["pdfPath"] = json!("Moved/Renamed.pdf");
        manifest["title"] = json!("Renamed");
        renamed[0] = upsert(
            format!("{ROOT}/manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        );
    }
    let second = service
        .sync_inkvault(
            "alice",
            "notes",
            request(first.server_head.clone(), renamed.clone()),
        )
        .await
        .unwrap();
    assert_eq!(second.status, SyncStatus::Ok);
    assert!(service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            second.server_head.as_ref().unwrap()
        )
        .await
        .is_err());
    let pdf = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Moved/Renamed.pdf",
            second.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    assert_eq!(
        lopdf::Document::load_mem(&pdf).unwrap().get_pages().len(),
        1
    );
    let replay = service
        .sync_inkvault("alice", "notes", request(first.server_head, renamed))
        .await
        .unwrap();
    assert_eq!(replay.server_head, second.server_head);
    let occupied = service
        .sync(
            "alice",
            "notes",
            request(
                second.server_head,
                vec![upsert("Notes/Test.pdf".into(), pdf)],
            ),
        )
        .await
        .unwrap();
    assert!(service
        .sync_inkvault(
            "alice",
            "notes",
            request(occupied.server_head.clone(), changes(3))
        )
        .await
        .is_err());
    assert_eq!(
        service
            .sync_inkvault("alice", "notes", request(None, vec![]))
            .await
            .unwrap()
            .server_head,
        occupied.server_head
    );
}

#[tokio::test]
async fn annotation_can_move_before_first_publication_and_keeps_its_original_base() {
    let (_dir, service, initial) = setup().await;
    let first = service
        .sync_inkvault("alice", "notes", request(initial, changes(1)))
        .await
        .unwrap();
    let pdf = service
        .file_bytes_at_version(
            "alice",
            "notes",
            "Notes/Test.pdf",
            first.server_head.as_ref().unwrap(),
        )
        .await
        .unwrap()
        .1;
    let imported = service
        .sync(
            "alice",
            "notes",
            request(
                first.server_head,
                vec![upsert("Imported.pdf".into(), pdf.clone())],
            ),
        )
        .await
        .unwrap();
    let root = ".inkvault/notes/33333333-3333-4333-8333-333333333333";
    let mut head = imported.server_head;
    for (rev, destination) in [(1, "Moved/Annotation.pdf"), (2, "Moved/Again.pdf")] {
        let mut package = changes(rev);
        for change in &mut package {
            if let ClientChange::Upsert {
                path,
                content_base64,
                sha256,
                ..
            } = change
            {
                *path = path.replace(ROOT, root);
                if path.ends_with("manifest.json") {
                    let mut manifest: Value = serde_json::from_slice(
                        &STANDARD.decode(content_base64.as_ref().unwrap()).unwrap(),
                    )
                    .unwrap();
                    manifest["documentId"] = json!(root.rsplit('/').next().unwrap());
                    manifest["pdfPath"] = json!(destination);
                    manifest["basePdfPath"] = json!("Imported.pdf");
                    manifest["basePdfHash"] = json!(sha256_hex(&pdf));
                    manifest["basePdfRevision"] = Value::Null;
                    let bytes = serde_json::to_vec(&manifest).unwrap();
                    *sha256 = Some(sha256_hex(&bytes));
                    *content_base64 = Some(STANDARD.encode(bytes));
                }
            }
        }
        let published = service
            .sync_inkvault("alice", "notes", request(head, package))
            .await
            .unwrap();
        assert_eq!(published.status, SyncStatus::Ok);
        head = published.server_head;
        let rendered = service
            .file_bytes_at_version("alice", "notes", destination, head.as_ref().unwrap())
            .await
            .unwrap()
            .1;
        assert_eq!(
            lopdf::Document::load_mem(&rendered)
                .unwrap()
                .get_pages()
                .len(),
            1
        );
        assert!(service
            .file_bytes_at_version("alice", "notes", "Imported.pdf", head.as_ref().unwrap())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn deleting_an_offline_only_note_is_an_idempotent_noop() {
    let (_dir, service, head) = setup().await;
    let deletes = vec![
        ClientChange::Delete {
            path: format!("{ROOT}/manifest.json"),
        },
        ClientChange::Delete {
            path: format!("{ROOT}/pages/{PAGE}.cbor"),
        },
    ];
    for _ in 0..2 {
        let result = service
            .sync_inkvault("alice", "notes", request(head.clone(), deletes.clone()))
            .await
            .unwrap();
        assert_eq!(result.status, SyncStatus::Ok);
        assert_eq!(result.server_head, head);
    }
}

#[test]
fn pressure_sensitive_marker_retains_transparency_and_variable_width() {
    use obsidian_git_sync_server::inkvault;
    use std::collections::BTreeMap;
    let mut files: BTreeMap<String, Vec<u8>> = changes(1)
        .iter()
        .map(|c| match c {
            ClientChange::Upsert {
                path,
                content_base64,
                ..
            } => (
                path.clone(),
                STANDARD.decode(content_base64.as_ref().unwrap()).unwrap(),
            ),
            _ => unreachable!(),
        })
        .collect();
    let mut manifest: Value =
        serde_json::from_slice(&files[&format!("{ROOT}/manifest.json")]).unwrap();
    let page = cbor(
        &json!({"schemaVersion":1,"width":210000,"height":297000,"strokes":[{"id":"marker","points":[[10000,10000,1,500,null],[20000,20000,2,500,null],[30000,30000,3,1000,null]],"style":{"tool":"marker","color":4294967040u64,"width":4000,"pressure":true}}],"objects":[],"tombstones":[]}),
    );
    manifest["pages"][0]["sha256"] = json!(sha256_hex(&page));
    files.insert(format!("{ROOT}/pages/{PAGE}.cbor"), page);
    let bytes = inkvault::render(&manifest, &files, None).unwrap();
    let doc = lopdf::Document::load_mem(&bytes).unwrap();
    let content =
        String::from_utf8(doc.get_page_content(*doc.get_pages().values().next().unwrap())).unwrap();
    assert!(content.contains("2000 w"));
    assert!(content.contains("4000 w"));
    assert!(content.contains(" gs"));
}
