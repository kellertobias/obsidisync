//! Native InkNote publication. All package pointers and the PDF change in one Git tree.
use super::*;
use crate::inkvault::{self as ink, array, integer, text};
use anyhow::ensure;
use serde_json::Value;
use tokio::process::Command;

#[derive(Serialize, Deserialize)]
struct Publication {
    old_head: Option<String>,
    new_head: String,
    manifest: BinaryManifest,
}

pub fn change_path(change: &ClientChange) -> &str {
    match change {
        ClientChange::Upsert { path, .. } | ClientChange::Delete { path } => path,
    }
}
pub fn file_path(change: &ServerFileChange) -> &str {
    match change {
        ServerFileChange::Upsert { path, .. } | ServerFileChange::Delete { path } => path,
    }
}

static PUBLICATION_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

impl VaultService {
    /// Ordinary callers cannot overwrite one half of a managed note.
    pub(super) async fn guard_inkvault_paths(
        &self,
        repo: &Path,
        binary: &Path,
        paths: &[String],
    ) -> Result<()> {
        let ledger = read_manifest(repo).await?;
        let mut protected = Vec::new();
        for (path, entry) in &ledger.files {
            if ink::is_source(path) && path.ends_with("/manifest.json") {
                let m: Value = serde_json::from_slice(&read_binary_object(binary, entry).await?)?;
                protected.push(text(&m, "pdfPath")?.to_string());
            }
        }
        for path in paths {
            ensure!(
                !ink::is_source(path)
                    && !protected
                        .iter()
                        .any(|p| p == path || p.starts_with(&format!("{path}/"))),
                "InkNote pair requires inkVaultNotesV1: {path}"
            );
        }
        Ok(())
    }

    /// One complete document per request. Ordinary files use the regular sync endpoint.
    pub async fn sync_inkvault(
        &self,
        user: &str,
        vault: &str,
        request: SyncRequest,
    ) -> Result<SyncResponse> {
        self.sync_inkvault_inner(user, vault, request, false).await
    }
    pub async fn resolve_inkvault(
        &self,
        user: &str,
        vault: &str,
        request: SyncRequest,
    ) -> Result<SyncResponse> {
        ensure!(
            request.base_head.is_some() && !request.changes.is_empty(),
            "paired resolution requires expected server head and complete source"
        );
        self.sync_inkvault_inner(user, vault, request, true).await
    }
    async fn sync_inkvault_inner(
        &self,
        user: &str,
        vault: &str,
        request: SyncRequest,
        resolving: bool,
    ) -> Result<SyncResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.with_lock(&user, &vault, || {
            self.sync_inkvault_locked(&user, &vault, request, resolving)
        })
        .await
    }
    async fn sync_inkvault_locked(
        &self,
        user: &str,
        vault: &str,
        request: SyncRequest,
        resolving: bool,
    ) -> Result<SyncResponse> {
        let _slot = PUBLICATION_SLOT.acquire().await?;
        let repo = self.repo_dir(user, vault);
        let binary = self.binary_dir(user, vault);
        let state = self.read_state(user, vault).await?;
        self.configure_git(&repo, &state).await?;
        // Never text-merge the ledger. A remote divergence requires explicit reconciliation.
        if state.uses_remote() {
            self.fetch(&repo).await?;
            let remote = format!("origin/{}", state.branch);
            let exists = git(Some(&repo), &["rev-parse", "--verify", &remote], &[0, 128]).await?;
            if exists.code == 0 {
                let result =
                    git(Some(&repo), &["merge", "--ff-only", &remote], &[0, 1, 128]).await?;
                ensure!(
                    result.code == 0,
                    "InkNote remote diverged; reconcile the vault before publishing"
                );
            }
        }
        let head = self.head_from_repo(&repo).await?;
        let base = validate_optional_commit_id(request.base_head.as_deref())?;
        if let Some(ref base) = base {
            ensure!(
                self.valid_commit(&repo, base).await?,
                "unknown InkNote base revision"
            );
        }
        ensure!(
            !resolving || base == head,
            "InkNote changed since conflict was shown; synchronize again"
        );
        let current = read_manifest(&repo).await?;
        if request.changes.is_empty() {
            if state.uses_remote() {
                git(
                    Some(&repo),
                    &[
                        "push",
                        "origin",
                        &format!("HEAD:refs/heads/{}", state.branch),
                    ],
                    &[0],
                )
                .await?;
            }
            return Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: head,
                files: self
                    .changed_files_since(
                        &repo,
                        &binary,
                        base.as_deref(),
                        &request.client_manifest,
                        request.file_content.is_inline(),
                        true,
                    )
                    .await?,
                conflicts: vec![],
            });
        }
        let root = ink::document_root(change_path(&request.changes[0]))?;
        let prefix = format!("{root}/");
        let manifest_path = format!("{root}/manifest.json");
        let mut files = BTreeMap::new();
        let mut bytes = 0usize;
        for (path, entry) in &current.files {
            if path.starts_with(&prefix) {
                bytes = bytes
                    .checked_add(entry.size as usize)
                    .ok_or_else(|| anyhow!("package too large"))?;
                ensure!(bytes <= 256 * 1024 * 1024, "package exceeds 256 MiB");
                files.insert(path.clone(), read_binary_object(&binary, entry).await?);
            }
        }
        let old_manifest = files
            .get(&manifest_path)
            .map(|b| ink::manifest(b, &root))
            .transpose()?;
        let mut touched = BTreeSet::new();
        for change in &request.changes {
            let path = change_path(change);
            ensure!(
                ink::document_root(path)? == root
                    && path.starts_with(&prefix)
                    && touched.insert(path.to_string()),
                "request must contain unique paths in one InkNote"
            );
            match change {
                ClientChange::Upsert {
                    content_base64,
                    upload_id,
                    sha256,
                    ..
                } => {
                    let content = self
                        .content_from_inline_or_upload(
                            &self.upload_dir(user, vault),
                            path,
                            content_base64.as_ref(),
                            upload_id.as_ref(),
                        )
                        .await?;
                    ensure!(
                        sha256.as_deref() == Some(sha256_hex(&content).as_str()),
                        "InkNote checksum mismatch"
                    );
                    bytes = bytes
                        .saturating_sub(files.get(path).map_or(0, Vec::len))
                        .saturating_add(content.len());
                    ensure!(bytes <= 256 * 1024 * 1024, "package exceeds 256 MiB");
                    files.insert(path.to_string(), content);
                }
                ClientChange::Delete { .. } => {
                    files.remove(path);
                }
            }
        }
        ensure!(
            touched.contains(&manifest_path),
            "publish the manifest with every InkNote change"
        );
        let previous = if let Some(ref base) = base {
            self.binary_manifest_at(&repo, base).await?
        } else {
            BinaryManifest::default()
        };
        let deleting = !files.contains_key(&manifest_path);
        if deleting
            && old_manifest.is_none()
            && !previous.files.contains_key(&manifest_path)
            && !current.files.keys().any(|p| p.starts_with(&prefix))
            && files.is_empty()
            && request
                .changes
                .iter()
                .all(|c| matches!(c, ClientChange::Delete { .. }))
        {
            // A document created and deleted offline has never had a server-side pair.
            return Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: head,
                files: self
                    .changed_files_since(
                        &repo,
                        &binary,
                        base.as_deref(),
                        &request.client_manifest,
                        request.file_content.is_inline(),
                        true,
                    )
                    .await?,
                conflicts: vec![],
            });
        }
        let mut manifest = if deleting {
            if let Some(old) = &old_manifest {
                old.clone()
            } else {
                let entry = previous
                    .files
                    .get(&manifest_path)
                    .ok_or_else(|| anyhow!("note not found"))?;
                ink::manifest(&read_binary_object(&binary, entry).await?, &root)?
            }
        } else {
            ink::manifest(&files[&manifest_path], &root)?
        };
        let pdf_path = text(&manifest, "pdfPath")?.to_string();
        let base_pdf_path = manifest
            .get("basePdfPath")
            .and_then(Value::as_str)
            .unwrap_or(&pdf_path)
            .to_string();
        validate_vault_path(&base_pdf_path)?;
        if manifest.get("basePdfHash").is_some() && manifest["basePdfRevision"].is_null() {
            manifest["basePdfRevision"] = if let Some(old) = &old_manifest {
                ensure!(
                    old["basePdfHash"] == manifest["basePdfHash"],
                    "annotation base cannot change"
                );
                old["basePdfRevision"].clone()
            } else {
                let hash = text(&manifest, "basePdfHash")?;
                ensure!(
                    current
                        .files
                        .get(&base_pdf_path)
                        .is_some_and(|entry| entry.sha256 == hash),
                    "annotation base changed"
                );
                Value::String(
                    head.clone()
                        .ok_or_else(|| anyhow!("annotation base must be uploaded first"))?,
                )
            };
        }

        for (path, entry) in &current.files {
            if path != &manifest_path && ink::is_source(path) && path.ends_with("/manifest.json") {
                let other: Value =
                    serde_json::from_slice(&read_binary_object(&binary, entry).await?)?;
                ensure!(
                    text(&other, "pdfPath")? != pdf_path
                        && (old_manifest.is_some()
                            || manifest.get("basePdfHash").is_none()
                            || text(&other, "pdfPath")? != base_pdf_path),
                    "PDF belongs to another InkNote"
                );
            }
        }
        ensure!(files.len() <= 4096, "InkNote exceeds 4096 source files");
        let old_pdf = old_manifest
            .as_ref()
            .map(|m| text(m, "pdfPath").map(str::to_string))
            .transpose()?
            .or_else(|| {
                (manifest.get("basePdfHash").is_some() && base_pdf_path != pdf_path)
                    .then(|| base_pdf_path.clone())
            });
        let pair_paths: BTreeSet<String> = current
            .files
            .keys()
            .chain(previous.files.keys())
            .chain(files.keys())
            .filter(|p| p.starts_with(&prefix))
            .cloned()
            .chain(std::iter::once(pdf_path.clone()))
            .chain(old_pdf.clone())
            .collect();
        let stale = pair_paths
            .iter()
            .any(|p| current.files.get(p) != previous.files.get(p));
        // Replaying an acknowledged-but-lost publication is safe, including server-derived renderRevision.
        let deleted_replay = deleting
            && files.is_empty()
            && pair_paths.iter().all(|p| !current.files.contains_key(p))
            && request
                .changes
                .iter()
                .all(|c| matches!(c, ClientChange::Delete { .. }));
        let same = deleted_replay
            || !deleting
                && old_manifest.as_ref().is_some_and(|old| {
                    let mut old = old.clone();
                    old.as_object_mut().unwrap().remove("renderRevision");
                    let mut new = manifest.clone();
                    new.as_object_mut().unwrap().remove("renderRevision");
                    old == new
                })
                && files
                    .iter()
                    .filter(|(p, _)| *p != &manifest_path)
                    .all(|(p, b)| {
                        current
                            .files
                            .get(p)
                            .is_some_and(|e| e.sha256 == sha256_hex(b))
                    })
                && current
                    .files
                    .keys()
                    .filter(|p| p.starts_with(&prefix))
                    .all(|p| files.contains_key(p));
        if stale && !same {
            let mut remote = Vec::new();
            for path in &pair_paths {
                remote.push(match current.files.get(path) {
                    Some(entry) => ServerFileChange::Upsert {
                        path: path.clone(),
                        sha256: entry.sha256.clone(),
                        size: Some(entry.size),
                        content_base64: if request.file_content.is_inline() {
                            Some(STANDARD.encode(read_binary_object(&binary, entry).await?))
                        } else {
                            None
                        },
                    },
                    None => ServerFileChange::Delete { path: path.clone() },
                });
            }
            return Ok(SyncResponse {
                status: SyncStatus::Conflict,
                server_head: head,
                files: remote,
                conflicts: pair_paths
                    .into_iter()
                    .map(|path| SyncConflict {
                        path,
                        reason: format!("InkNote pair changed: {root}"),
                    })
                    .collect(),
            });
        }
        if !same {
            if old_pdf.as_ref().is_some_and(|p| p != &pdf_path) {
                ensure!(
                    !current.files.contains_key(&pdf_path) && !repo.join(&pdf_path).exists(),
                    "PDF destination already exists"
                );
            }
            let mut next = current.clone();
            next.files.retain(|p, _| !p.starts_with(&prefix));
            if let Some(old) = &old_pdf {
                next.files.remove(old);
            }
            if deleting {
                ensure!(files.is_empty(), "delete the complete InkNote package");
                next.files.remove(&pdf_path);
            } else {
                if let Some(old) = &old_manifest {
                    ensure!(
                        integer(&manifest, "sourceRevision")? > integer(old, "sourceRevision")?,
                        "sourceRevision must advance"
                    );
                }
                let mut base_bytes = None;
                if let Some(hash) = manifest.get("basePdfHash").and_then(Value::as_str) {
                    let revision = validate_commit_id(text(&manifest, "basePdfRevision")?)?;
                    let base_path = manifest
                        .get("basePdfPath")
                        .and_then(Value::as_str)
                        .unwrap_or(&pdf_path);
                    validate_vault_path(base_path)?;
                    let (_, original) = self
                        .file_bytes_at_version(user, vault, base_path, &revision)
                        .await?;
                    ensure!(
                        sha256_hex(&original) == hash,
                        "annotation base PDF checksum mismatch"
                    );
                    if let Some(old) = &old_manifest {
                        let old_base_path = old
                            .get("basePdfPath")
                            .and_then(Value::as_str)
                            .unwrap_or(text(old, "pdfPath")?);
                        ensure!(
                            base_path == old_base_path,
                            "annotation base path cannot change"
                        );
                        ensure!(
                            old["basePdfHash"] == manifest["basePdfHash"]
                                && old["basePdfRevision"] == manifest["basePdfRevision"],
                            "annotation base cannot change"
                        );
                        let fixed = |m: &Value| -> Result<Vec<Value>> {
                            Ok(array(m, "pages")?
                                .iter()
                                .map(|p| serde_json::json!([p["id"], p["width"], p["height"]]))
                                .collect())
                        };
                        ensure!(
                            fixed(old)? == fixed(&manifest)?,
                            "annotation pages are fixed"
                        );
                    } else {
                        ensure!(
                            current
                                .files
                                .get(&base_pdf_path)
                                .is_some_and(|e| e.sha256 == hash),
                            "annotation base changed"
                        );
                    }
                    base_bytes = Some(original);
                } else {
                    ensure!(
                        old_manifest.is_some()
                            || !current.files.contains_key(&pdf_path)
                                && !repo.join(&pdf_path).exists(),
                        "PDF path already exists"
                    );
                }
                for (path, content) in &files {
                    let rel = path.strip_prefix(&prefix).unwrap();
                    if let Some(asset) = rel.strip_prefix("assets/") {
                        let hash = asset.split('.').next().unwrap_or_default();
                        ensure!(hash == sha256_hex(content), "asset hash/path mismatch");
                    } else if rel != "manifest.json" {
                        ensure!(
                            rel.starts_with("pages/")
                                && rel.ends_with(".cbor")
                                && ink::uuid(
                                    rel.trim_start_matches("pages/").trim_end_matches(".cbor")
                                ),
                            "unsupported InkNote package path"
                        );
                    }
                }
                // Hash all authoritative source content in sorted order; renderRevision itself is excluded.
                manifest.as_object_mut().unwrap().remove("renderRevision");
                files.insert(manifest_path.clone(), serde_json::to_vec(&manifest)?);
                let mut digest = Sha256::new();
                digest.update(b"InkVault renderer v1\0");
                for (path, bytes) in &files {
                    digest.update((path.len() as u64).to_be_bytes());
                    digest.update(path.as_bytes());
                    digest.update((bytes.len() as u64).to_be_bytes());
                    digest.update(bytes);
                }
                manifest["renderRevision"] = Value::String(format!("{:x}", digest.finalize()));
                files.insert(manifest_path.clone(), serde_json::to_vec(&manifest)?);
                let status_path = self.vault_dir(user, vault).join("inkvault-render.json");
                durable_write(&status_path,&serde_json::to_vec(&serde_json::json!({"documentId":manifest["documentId"],"sourceRevision":manifest["sourceRevision"],"status":"pending"}))?).await?;
                let render_manifest = manifest.clone();
                let render_files = files.clone();
                let rendered = tokio::task::spawn_blocking(move || {
                    ink::render(&render_manifest, &render_files, base_bytes.as_deref())
                })
                .await?;
                let pdf = match rendered {
                    Ok(pdf) => pdf,
                    Err(error) => {
                        durable_write(&status_path,&serde_json::to_vec(&serde_json::json!({"documentId":manifest["documentId"],"sourceRevision":manifest["sourceRevision"],"status":"failed","error":error.to_string()}))?).await?;
                        return Err(anyhow!("InkNote render failed: {error}"));
                    }
                };
                let mtime = integer(&manifest, "modified")?;
                for (path, content) in &files {
                    next.files.insert(
                        path.clone(),
                        store_binary(&binary, path, content, mtime).await?,
                    );
                }
                next.files.insert(
                    pdf_path.clone(),
                    store_binary(&binary, &pdf_path, &pdf, mtime).await?,
                );
            }
            self.publish_inkvault(user, vault, next).await?;
        }
        if state.uses_remote() {
            git(
                Some(&repo),
                &[
                    "push",
                    "origin",
                    &format!("HEAD:refs/heads/{}", state.branch),
                ],
                &[0],
            )
            .await?;
        }
        if let Some(head) = self.head_from_repo(&repo).await? {
            let mut touched = TouchedPaths::default();
            for change in &request.changes {
                match change {
                    ClientChange::Upsert { path, .. } => touched.upserts.push(path.clone()),
                    ClientChange::Delete { path } => touched.deletes.push(path.clone()),
                }
            }
            self.upsert_device(
                user,
                vault,
                &request.client_id,
                &request.device_name,
                &head,
                &touched,
            )
            .await?;
        }
        Ok(SyncResponse {
            status: SyncStatus::Ok,
            server_head: self.head_from_repo(&repo).await?,
            files: self
                .changed_files_since(
                    &repo,
                    &binary,
                    base.as_deref(),
                    &[],
                    request.file_content.is_inline(),
                    true,
                )
                .await?,
            conflicts: vec![],
        })
    }

    async fn publish_inkvault(
        &self,
        user: &str,
        vault: &str,
        manifest: BinaryManifest,
    ) -> Result<()> {
        let repo = self.repo_dir(user, vault);
        let dir = self.vault_dir(user, vault);
        let old_head = self.head_from_repo(&repo).await?;
        let index = dir.join("inkvault-index");
        let _ = fs::remove_file(&index).await;
        index_git(
            &repo,
            &index,
            &["read-tree", old_head.as_deref().unwrap_or("--empty")],
        )
        .await?;
        let payload = dir.join("inkvault-manifest");
        durable_write(&payload, &serde_json::to_vec_pretty(&manifest)?).await?;
        let hash = git(
            Some(&repo),
            &[
                "-c",
                "core.fsync=all",
                "hash-object",
                "-w",
                path_to_str(&payload)?,
            ],
            &[0],
        )
        .await?;
        let hash = String::from_utf8(hash.stdout)?.trim().to_string();
        index_git(
            &repo,
            &index,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                &hash,
                BINARY_MANIFEST_PATH,
            ],
        )
        .await?;
        let tree = index_git(&repo, &index, &["write-tree"]).await?;
        let mut args = vec![
            "-c",
            "core.fsync=all",
            "commit-tree",
            tree.trim(),
            "-m",
            "sync: publish InkNote source and PDF",
        ];
        if let Some(ref parent) = old_head {
            args.extend(["-p", parent]);
        }
        let commit = git(Some(&repo), &args, &[0]).await?;
        let publication = Publication {
            old_head,
            new_head: String::from_utf8(commit.stdout)?.trim().to_string(),
            manifest,
        };
        durable_write(
            &dir.join("inkvault-publication.json"),
            &serde_json::to_vec(&publication)?,
        )
        .await?;
        self.recover_inkvault(user, vault).await
    }

    pub(super) async fn recover_inkvault(&self, user: &str, vault: &str) -> Result<()> {
        let path = self
            .vault_dir(user, vault)
            .join("inkvault-publication.json");
        let bytes = match fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let publication: Publication = serde_json::from_slice(&bytes)?;
        let repo = self.repo_dir(user, vault);
        validate_commit_id(&publication.new_head)?;
        ensure!(
            self.binary_manifest_at(&repo, &publication.new_head)
                .await?
                == publication.manifest,
            "InkNote recovery journal differs from committed ledger"
        );
        let head = self.head_from_repo(&repo).await?;
        ensure!(
            head == publication.old_head || head.as_deref() == Some(&publication.new_head),
            "InkNote recovery found an unexpected Git head"
        );
        if head == publication.old_head {
            git(
                Some(&repo),
                &[
                    "-c",
                    "core.fsync=all",
                    "update-ref",
                    "HEAD",
                    &publication.new_head,
                    publication
                        .old_head
                        .as_deref()
                        .unwrap_or("0000000000000000000000000000000000000000"),
                ],
                &[0],
            )
            .await?;
        }
        write_manifest(&repo, &publication.manifest).await?;
        git(
            Some(&repo),
            &["reset", "HEAD", "--", BINARY_MANIFEST_PATH],
            &[0],
        )
        .await?;
        durable_write(
            &self.vault_dir(user, vault).join("inkvault-render.json"),
            &serde_json::to_vec(
                &serde_json::json!({"status":"ready","serverHead":publication.new_head}),
            )?,
        )
        .await?;
        fs::remove_file(&path).await?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
}

pub(crate) async fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!("{}.tmp", random_upload_id()?));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&tmp, path).await?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
async fn index_git(repo: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsync=all"])
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(repo)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "InkNote Git transaction failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
