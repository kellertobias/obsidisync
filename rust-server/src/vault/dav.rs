//! Direct file operations used by the WebDAV endpoint.
//!
//! WebDAV clients (e-ink tablets, desktop file managers) do not participate in the plugin's
//! manifest-based sync protocol. They read and write individual files, and every write becomes a
//! regular commit in the vault repository so the Obsidian clients pick it up on their next sync
//! through the normal `changed_files_since` path. Text files live in the git working tree; binary
//! files (PDFs, images) go to the binary object store and the committed binary manifest, exactly
//! as they would when uploaded by the plugin.

use super::*;
use crate::binary_store::BinaryEntry;
use crate::time_format::unix_now_millis;
use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

/// A file or directory as seen through WebDAV. `path` is vault-relative; the vault root is `""`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_millis: i64,
    pub etag: String,
}

/// Content for a WebDAV write: either in memory or already streamed to a staged file.
#[derive(Debug)]
pub enum WriteSource {
    Bytes(Vec<u8>),
    /// A file created by `dav_stage_upload`; it is consumed (moved or removed) by the write.
    File(PathBuf),
}

/// The device performing a WebDAV write, recorded in commit subjects and the device registry.
#[derive(Debug, Clone)]
pub struct DavDevice {
    pub client_id: String,
    pub name: String,
}

impl DavEntry {
    pub fn dir(path: &str, mtime_millis: i64) -> Self {
        Self {
            path: path.to_string(),
            is_dir: true,
            size: 0,
            mtime_millis,
            etag: String::new(),
        }
    }

    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

impl VaultService {
    pub async fn is_registered(&self, user: &str, vault: &str) -> bool {
        let (Ok(user), Ok(vault)) = (validate_slug(user, "user"), validate_slug(vault, "vault"))
        else {
            return false;
        };
        fs::metadata(self.vault_dir(&user, &vault).join("state.json"))
            .await
            .is_ok()
    }

    pub async fn dav_stat(&self, user: &str, vault: &str, path: &str) -> Result<Option<DavEntry>> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let path = validate_dav_path(path)?;
        self.with_lock(&user, &vault, || async {
            self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let manifest = read_manifest(&repo).await?;
            stat_unlocked(&repo, &manifest, &path).await
        })
        .await
    }

    pub async fn dav_list(&self, user: &str, vault: &str, dir: &str) -> Result<Vec<DavEntry>> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let dir = validate_dav_path(dir)?;
        self.with_lock(&user, &vault, || async {
            self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let manifest = read_manifest(&repo).await?;
            list_unlocked(&repo, &manifest, &dir).await
        })
        .await
    }

    pub async fn dav_read(
        &self,
        user: &str,
        vault: &str,
        path: &str,
    ) -> Result<(DavEntry, Vec<u8>)> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let path = validate_dav_path(path)?;
        self.with_lock(&user, &vault, || async {
            self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let manifest = read_manifest(&repo).await?;
            let entry = stat_unlocked(&repo, &manifest, &path)
                .await?
                .ok_or_else(|| anyhow!("not found: {path}"))?;
            if entry.is_dir {
                bail!("not found: {path} is a directory");
            }
            let content = match manifest.files.get(&path) {
                Some(binary) => read_binary_object(&self.binary_dir(&user, &vault), binary).await?,
                None => fs::read(repo_path(&repo, &path)?).await?,
            };
            Ok((entry, content))
        })
        .await
    }

    /// Allocates a scratch file inside the vault's upload directory. Callers stream a request
    /// body into it and then hand it to `dav_write_from_file`, so large PDFs never sit in memory.
    pub async fn dav_stage_upload(&self, user: &str, vault: &str) -> Result<PathBuf> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.read_registered_state(&user, &vault).await?;
        let upload_dir = self.upload_dir(&user, &vault);
        fs::create_dir_all(&upload_dir).await?;
        let staged = upload_dir.join(format!("dav-{}.bin", random_upload_id()?));
        fs::write(&staged, []).await?;
        Ok(staged)
    }

    /// Writes a whole file from memory. Returns `true` when the file did not exist before.
    pub async fn dav_write(
        &self,
        user: &str,
        vault: &str,
        path: &str,
        content: Vec<u8>,
        device: &DavDevice,
    ) -> Result<bool> {
        self.dav_write_source(user, vault, path, WriteSource::Bytes(content), device)
            .await
    }

    /// Writes a whole file from a staged upload. The staged file is always cleaned up.
    pub async fn dav_write_from_file(
        &self,
        user: &str,
        vault: &str,
        path: &str,
        staged: &Path,
        device: &DavDevice,
    ) -> Result<bool> {
        let result = self
            .dav_write_source(
                user,
                vault,
                path,
                WriteSource::File(staged.to_path_buf()),
                device,
            )
            .await;
        let _ = fs::remove_file(staged).await;
        result
    }

    async fn dav_write_source(
        &self,
        user: &str,
        vault: &str,
        path: &str,
        source: WriteSource,
        device: &DavDevice,
    ) -> Result<bool> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let path = validate_dav_path(path)?;
        if path.is_empty() {
            bail!("forbidden: cannot write the vault root");
        }
        self.with_lock(&user, &vault, || async {
            let state = self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            let mut manifest = read_manifest(&repo).await?;
            let existing = stat_unlocked(&repo, &manifest, &path).await?;
            if existing.as_ref().is_some_and(|entry| entry.is_dir) {
                bail!("conflict: {path} is a directory");
            }
            write_unlocked(&repo, &binary_root, &mut manifest, &path, source).await?;
            write_manifest(&repo, &manifest).await?;
            self.clear_pending_conflicts(&user, &vault, std::slice::from_ref(&path))
                .await?;
            let touched = TouchedPaths {
                upserts: vec![path.clone()],
                deletes: vec![],
            };
            self.finish_dav_commit(&user, &vault, &state, &repo, device, &touched)
                .await?;
            Ok(existing.is_none())
        })
        .await
    }

    /// Deletes a file, or a directory with everything below it.
    pub async fn dav_delete(
        &self,
        user: &str,
        vault: &str,
        path: &str,
        device: &DavDevice,
    ) -> Result<()> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let path = validate_dav_path(path)?;
        if path.is_empty() {
            bail!("forbidden: cannot delete the vault root");
        }
        self.with_lock(&user, &vault, || async {
            let state = self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let mut manifest = read_manifest(&repo).await?;
            let entry = stat_unlocked(&repo, &manifest, &path)
                .await?
                .ok_or_else(|| anyhow!("not found: {path}"))?;
            let files = if entry.is_dir {
                files_under(&repo, &manifest, &path).await?
            } else {
                vec![path.clone()]
            };
            for file in &files {
                delete_unlocked(&repo, &mut manifest, file).await?;
            }
            if entry.is_dir {
                let _ = fs::remove_dir_all(repo_path(&repo, &path)?).await;
            }
            write_manifest(&repo, &manifest).await?;
            self.clear_pending_conflicts(&user, &vault, &files).await?;
            let touched = TouchedPaths {
                upserts: vec![],
                deletes: files,
            };
            self.finish_dav_commit(&user, &vault, &state, &repo, device, &touched)
                .await
        })
        .await
    }

    /// Creates an empty directory in the working tree. Git does not track empty directories, so
    /// nothing is committed until a file lands inside; the directory is still visible over WebDAV.
    pub async fn dav_mkcol(&self, user: &str, vault: &str, path: &str) -> Result<()> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let path = validate_dav_path(path)?;
        if path.is_empty() {
            bail!("conflict: vault root already exists");
        }
        self.with_lock(&user, &vault, || async {
            self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let manifest = read_manifest(&repo).await?;
            if stat_unlocked(&repo, &manifest, &path).await?.is_some() {
                bail!("exists: {path}");
            }
            fs::create_dir_all(repo_path(&repo, &path)?).await?;
            Ok(())
        })
        .await
    }

    /// Moves or copies a file or directory. Returns `true` when the destination did not exist.
    #[allow(clippy::too_many_arguments)]
    pub async fn dav_move_or_copy(
        &self,
        user: &str,
        vault: &str,
        from: &str,
        to: &str,
        overwrite: bool,
        remove_source: bool,
        device: &DavDevice,
    ) -> Result<bool> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let from = validate_dav_path(from)?;
        let to = validate_dav_path(to)?;
        if from.is_empty() || to.is_empty() {
            bail!("forbidden: cannot move the vault root");
        }
        if from == to {
            bail!("forbidden: source and destination are the same");
        }
        if to.starts_with(&format!("{from}/")) {
            bail!("conflict: cannot move a directory into itself");
        }
        self.with_lock(&user, &vault, || async {
            let state = self.read_registered_state(&user, &vault).await?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            let mut manifest = read_manifest(&repo).await?;
            let source = stat_unlocked(&repo, &manifest, &from)
                .await?
                .ok_or_else(|| anyhow!("not found: {from}"))?;
            let destination = stat_unlocked(&repo, &manifest, &to).await?;
            if destination.is_some() && !overwrite {
                bail!("precondition failed: {to} already exists");
            }

            let mut touched = TouchedPaths::default();
            if let Some(destination) = &destination {
                let existing = if destination.is_dir {
                    files_under(&repo, &manifest, &to).await?
                } else {
                    vec![to.clone()]
                };
                for file in existing {
                    delete_unlocked(&repo, &mut manifest, &file).await?;
                    touched.deletes.push(file);
                }
                if destination.is_dir {
                    let _ = fs::remove_dir_all(repo_path(&repo, &to)?).await;
                }
            }

            let sources = if source.is_dir {
                files_under(&repo, &manifest, &from).await?
            } else {
                vec![from.clone()]
            };
            for file in &sources {
                let target = if source.is_dir {
                    format!("{to}{}", &file[from.len()..])
                } else {
                    to.clone()
                };
                let content = match manifest.files.get(file) {
                    Some(binary) => read_binary_object(&binary_root, binary).await?,
                    None => fs::read(repo_path(&repo, file)?).await?,
                };
                write_unlocked(
                    &repo,
                    &binary_root,
                    &mut manifest,
                    &target,
                    WriteSource::Bytes(content),
                )
                .await?;
                touched.upserts.push(target);
                if remove_source {
                    delete_unlocked(&repo, &mut manifest, file).await?;
                    touched.deletes.push(file.clone());
                }
            }
            if source.is_dir {
                if remove_source {
                    let _ = fs::remove_dir_all(repo_path(&repo, &from)?).await;
                } else {
                    fs::create_dir_all(repo_path(&repo, &to)?).await?;
                }
            }
            write_manifest(&repo, &manifest).await?;
            let mut cleared = touched.upserts.clone();
            cleared.extend(touched.deletes.iter().cloned());
            self.clear_pending_conflicts(&user, &vault, &cleared)
                .await?;
            self.finish_dav_commit(&user, &vault, &state, &repo, device, &touched)
                .await?;
            Ok(destination.is_none())
        })
        .await
    }

    async fn read_registered_state(&self, user: &str, vault: &str) -> Result<VaultState> {
        match fs::read(self.vault_dir(user, vault).join("state.json")).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("not found: vault {vault} is not registered")
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn finish_dav_commit(
        &self,
        user: &str,
        vault: &str,
        state: &VaultState,
        repo: &Path,
        device: &DavDevice,
        touched: &TouchedPaths,
    ) -> Result<()> {
        self.validate_remote_url(&state.remote_url)?;
        self.configure_git(repo, state).await?;
        self.commit_all_if_changed(
            repo,
            &format!(
                "sync: {} {}",
                sanitize_commit_component(&device.name),
                isoish_now()
            ),
        )
        .await?;
        if state.uses_remote() {
            self.fetch(repo).await?;
            if let Some(conflicts) = self.rebase_remote(repo, &state.branch).await? {
                self.record_pending_conflicts(user, vault, &device.client_id, &conflicts)
                    .await?;
                self.cleanup_conflict_state(repo).await?;
                bail!("conflict: remote changes conflict with this upload; resolve from Obsidian");
            }
            if let Some(conflicts) = self.push_after_rebase(repo, &state.branch).await? {
                self.record_pending_conflicts(user, vault, &device.client_id, &conflicts)
                    .await?;
                self.cleanup_conflict_state(repo).await?;
                bail!("conflict: remote changes conflict with this upload; resolve from Obsidian");
            }
        }
        if let Some(head) = self.head_from_repo(repo).await? {
            self.upsert_device(user, vault, &device.client_id, &device.name, &head, touched)
                .await?;
        }
        Ok(())
    }
}

/// Like `validate_vault_path`, but accepts `""` for the vault root and hides server metadata.
pub fn validate_dav_path(path: &str) -> Result<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let safe = validate_vault_path(trimmed)?;
    if is_hidden_path(&safe) {
        bail!("not found: {safe}");
    }
    Ok(safe)
}

fn is_hidden_path(path: &str) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".obsidian-git-sync"
        || path.starts_with(".obsidian-git-sync/")
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn manifest_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

fn text_etag(size: u64, mtime_millis: i64) -> String {
    format!("{size}-{mtime_millis}")
}

fn binary_entry(path: &str, entry: &crate::binary_store::BinaryEntry) -> DavEntry {
    DavEntry {
        path: path.to_string(),
        is_dir: false,
        size: entry.size,
        mtime_millis: entry.mtime,
        etag: entry.sha256.clone(),
    }
}

fn metadata_mtime_millis(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

async fn stat_unlocked(
    repo: &Path,
    manifest: &BinaryManifest,
    path: &str,
) -> Result<Option<DavEntry>> {
    if path.is_empty() {
        return Ok(Some(DavEntry::dir("", unix_now_millis())));
    }
    if let Some(entry) = manifest.files.get(path) {
        return Ok(Some(binary_entry(path, entry)));
    }
    match fs::metadata(repo_path(repo, path)?).await {
        Ok(metadata) if metadata.is_dir() => {
            return Ok(Some(DavEntry::dir(path, metadata_mtime_millis(&metadata))));
        }
        Ok(metadata) => {
            let mtime = metadata_mtime_millis(&metadata);
            return Ok(Some(DavEntry {
                path: path.to_string(),
                is_dir: false,
                size: metadata.len(),
                mtime_millis: mtime,
                etag: text_etag(metadata.len(), mtime),
            }));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let prefix = manifest_prefix(path);
    let newest = manifest
        .files
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(_, entry)| entry.mtime)
        .max();
    Ok(newest.map(|mtime| DavEntry::dir(path, mtime)))
}

async fn list_unlocked(repo: &Path, manifest: &BinaryManifest, dir: &str) -> Result<Vec<DavEntry>> {
    let mut children: BTreeMap<String, DavEntry> = BTreeMap::new();
    match fs::read_dir(repo_path(repo, dir).unwrap_or_else(|_| repo.to_path_buf())).await {
        Ok(mut entries) => {
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().to_string();
                let child = join_path(dir, &name);
                if is_hidden_path(&child) {
                    continue;
                }
                let metadata = entry.metadata().await?;
                let mtime = metadata_mtime_millis(&metadata);
                let dav_entry = if metadata.is_dir() {
                    DavEntry::dir(&child, mtime)
                } else {
                    DavEntry {
                        path: child.clone(),
                        is_dir: false,
                        size: metadata.len(),
                        mtime_millis: mtime,
                        etag: text_etag(metadata.len(), mtime),
                    }
                };
                children.insert(name, dav_entry);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let prefix = manifest_prefix(dir);
    for (key, entry) in &manifest.files {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((first, _)) => {
                let child = join_path(dir, first);
                let existing = children
                    .entry(first.to_string())
                    .or_insert_with(|| DavEntry::dir(&child, entry.mtime));
                if existing.is_dir && existing.mtime_millis < entry.mtime {
                    existing.mtime_millis = entry.mtime;
                }
            }
            None => {
                children.insert(rest.to_string(), binary_entry(key, entry));
            }
        }
    }
    Ok(children.into_values().collect())
}

async fn files_under(repo: &Path, manifest: &BinaryManifest, dir: &str) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_string()];
    while let Some(current) = pending.pop() {
        for entry in list_unlocked(repo, manifest, &current).await? {
            if entry.is_dir {
                pending.push(entry.path);
            } else {
                files.push(entry.path);
            }
        }
    }
    files.sort();
    Ok(files)
}

async fn write_unlocked(
    repo: &Path,
    binary_root: &Path,
    manifest: &mut BinaryManifest,
    path: &str,
    source: WriteSource,
) -> Result<()> {
    if is_text_or_code_path(path) {
        let content = match source {
            WriteSource::Bytes(content) => content,
            WriteSource::File(staged) => {
                let content = fs::read(&staged).await?;
                let _ = fs::remove_file(&staged).await;
                content
            }
        };
        manifest.files.remove(path);
        return write_repo_file(repo, path, &content).await;
    }

    let staged = match source {
        WriteSource::File(staged) => staged,
        WriteSource::Bytes(content) => {
            fs::create_dir_all(binary_root).await?;
            let staged = binary_root.join(format!("staging-{}.bin", random_upload_id()?));
            fs::write(&staged, &content).await?;
            staged
        }
    };
    let result = store_staged_binary(binary_root, manifest, path, &staged).await;
    let _ = fs::remove_file(&staged).await;
    result
}

/// Moves a staged file into the content-addressed binary store without reading it into memory.
async fn store_staged_binary(
    binary_root: &Path,
    manifest: &mut BinaryManifest,
    path: &str,
    staged: &Path,
) -> Result<()> {
    let sha256 = sha256_file(staged).await?;
    // Many tablets re-upload every note on each sync. Keep the existing entry when the bytes
    // are identical so an unchanged file does not produce a new commit and a re-download on
    // every Obsidian client.
    if manifest
        .files
        .get(path)
        .is_some_and(|existing| existing.sha256 == sha256)
    {
        return Ok(());
    }
    let size = fs::metadata(staged).await?.len();
    let object_path = format!("{}/{}", &sha256[0..2], sha256);
    let absolute = binary_root.join(&object_path);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).await?;
    }
    if fs::metadata(&absolute).await.is_err() && fs::rename(staged, &absolute).await.is_err() {
        fs::copy(staged, &absolute).await?;
    }
    manifest.files.insert(
        path.to_string(),
        BinaryEntry {
            sha256,
            mtime: unix_now_millis(),
            size,
            object_path,
        },
    );
    Ok(())
}

async fn delete_unlocked(repo: &Path, manifest: &mut BinaryManifest, path: &str) -> Result<()> {
    manifest.files.remove(path);
    match fs::remove_file(repo_path(repo, path)?).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
