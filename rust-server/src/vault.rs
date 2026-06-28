use crate::binary_store::{
    read_binary_object, read_manifest, sha256_hex, store_binary, validate_manifest, write_manifest,
    BinaryManifest, BINARY_MANIFEST_PATH,
};
use crate::git::{git, git_strings};
use crate::paths::{
    is_text_or_code_path, repo_path, sanitize_commit_component, validate_commit_id,
    validate_git_branch, validate_git_identity, validate_slug, validate_vault_path,
};
use crate::protocol::*;
use crate::remote::RemotePolicy;
use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

const UPLOAD_CHUNK_SIZE_BYTES: u64 = 512 * 1024;
const PENDING_CONFLICTS_PATH: &str = "pending-conflicts.json";
const PENDING_CONFLICT_REASON: &str = "file is already awaiting conflict resolution";

#[derive(Debug, Clone)]
pub struct VaultService {
    pub data_dir: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    remote_policy: RemotePolicy,
}

#[derive(Debug, Clone)]
pub struct VaultServiceOptions {
    pub data_dir: PathBuf,
    pub remote_policy: RemotePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultState {
    pub user: String,
    pub vault: String,
    pub remote_url: String,
    pub branch: String,
    pub author_name: String,
    pub author_email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadState {
    path: String,
    sha256: String,
    size: u64,
    received: u64,
    complete: bool,
}

impl VaultState {
    fn uses_remote(&self) -> bool {
        !self.remote_url.trim().is_empty()
    }
}

impl VaultService {
    pub fn new(data_dir: PathBuf) -> Self {
        Self::new_with_options(VaultServiceOptions {
            data_dir,
            remote_policy: RemotePolicy::default(),
        })
    }

    pub fn new_for_tests(data_dir: PathBuf) -> Self {
        Self::new_with_options(VaultServiceOptions {
            data_dir,
            remote_policy: RemotePolicy {
                allow_local_remotes: true,
                allowed_hosts: Vec::new(),
            },
        })
    }

    pub fn new_with_options(options: VaultServiceOptions) -> Self {
        Self {
            data_dir: options.data_dir,
            locks: Arc::new(Mutex::new(HashMap::new())),
            remote_policy: options.remote_policy,
        }
    }

    pub async fn register(
        &self,
        user: &str,
        vault: &str,
        request: RegisterRequest,
    ) -> Result<RegisterResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.with_lock(&user, &vault, || async {
            let remote_url = request.remote_url.trim().to_string();
            self.validate_remote_url(&remote_url)?;
            let branch = validate_git_branch(&request.branch)?;
            let state = VaultState {
                user: user.clone(),
                vault: vault.clone(),
                remote_url,
                branch: branch.clone(),
                author_name: validate_git_identity(&request.author_name, "author name")?,
                author_email: validate_git_identity(&request.author_email, "author email")?,
            };
            self.ensure_repo(&state).await?;
            self.write_state(&state).await?;
            Ok(RegisterResponse {
                user: user.clone(),
                vault: vault.clone(),
                server_head: self
                    .head_from_repo(&self.repo_dir(&state.user, &state.vault))
                    .await?,
                branch,
            })
        })
        .await
    }

    pub async fn init_upload(
        &self,
        user: &str,
        vault: &str,
        request: UploadInitRequest,
    ) -> Result<UploadInitResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.with_lock(&user, &vault, || async {
            let path = validate_vault_path(&request.path)?;
            let sha256 = validate_sha256_hex(&request.sha256)?;
            if request.size > i64::MAX as u64 {
                bail!("invalid upload size");
            }

            let upload_dir = self.upload_dir(&user, &vault);
            fs::create_dir_all(&upload_dir).await?;
            let upload_id = self.new_upload_id(&upload_dir).await?;
            let state = UploadState {
                path,
                sha256,
                size: request.size,
                received: 0,
                complete: false,
            };
            fs::write(self.upload_content_path(&user, &vault, &upload_id), []).await?;
            self.write_upload_state(&user, &vault, &upload_id, &state)
                .await?;
            Ok(UploadInitResponse {
                upload_id,
                chunk_size: UPLOAD_CHUNK_SIZE_BYTES,
            })
        })
        .await
    }

    pub async fn append_upload_chunk(
        &self,
        user: &str,
        vault: &str,
        upload_id: &str,
        request: UploadChunkRequest,
    ) -> Result<UploadChunkResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let upload_id = validate_upload_id(upload_id)?;
        self.with_lock(&user, &vault, || async {
            let mut state = self.read_upload_state(&user, &vault, &upload_id).await?;
            if state.complete {
                bail!("upload is already complete");
            }
            if request.offset != state.received {
                bail!("invalid upload offset");
            }

            let content = STANDARD.decode(request.content_base64.as_bytes())?;
            if content.is_empty() {
                bail!("invalid upload chunk");
            }
            let received = state
                .received
                .checked_add(content.len() as u64)
                .ok_or_else(|| anyhow!("invalid upload size"))?;
            if received > state.size {
                bail!("upload exceeds declared size");
            }

            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(self.upload_content_path(&user, &vault, &upload_id))
                .await?;
            file.write_all(&content).await?;
            state.received = received;
            self.write_upload_state(&user, &vault, &upload_id, &state)
                .await?;
            Ok(UploadChunkResponse {
                upload_id,
                received,
            })
        })
        .await
    }

    pub async fn complete_upload(
        &self,
        user: &str,
        vault: &str,
        upload_id: &str,
    ) -> Result<UploadCompleteResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let upload_id = validate_upload_id(upload_id)?;
        self.with_lock(&user, &vault, || async {
            let mut state = self.read_upload_state(&user, &vault, &upload_id).await?;
            if state.received != state.size {
                bail!("upload is incomplete");
            }
            let actual = sha256_file(&self.upload_content_path(&user, &vault, &upload_id)).await?;
            if actual != state.sha256 {
                bail!("upload checksum mismatch");
            }
            state.complete = true;
            self.write_upload_state(&user, &vault, &upload_id, &state)
                .await?;
            Ok(UploadCompleteResponse {
                upload_id,
                size: state.size,
                sha256: state.sha256,
            })
        })
        .await
    }

    pub async fn sync(
        &self,
        user: &str,
        vault: &str,
        request: SyncRequest,
    ) -> Result<SyncResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.with_lock(&user, &vault, || async {
            let state = self.read_state(&user, &vault).await?;
            self.validate_remote_url(&state.remote_url)?;
            let base_head = validate_optional_commit_id(request.base_head.as_deref())?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            let upload_root = self.upload_dir(&user, &vault);
            self.configure_git(&repo, &state).await?;
            if state.uses_remote() {
                self.fetch(&repo).await?;
            }
            self.commit_all_if_changed(&repo, "sync: server pending")
                .await?;
            if state.uses_remote() {
                if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                    return self
                        .conflict_response(
                            &user,
                            &vault,
                            &repo,
                            &binary_root,
                            base_head.as_deref(),
                            conflicts,
                        )
                        .await;
                }
            }

            let conflicts = self
                .apply_client_changes(
                    &user,
                    &vault,
                    &repo,
                    &binary_root,
                    &upload_root,
                    base_head.as_deref(),
                    &request.changes,
                )
                .await?;
            if !conflicts.is_empty() {
                return self
                    .conflict_response(
                        &user,
                        &vault,
                        &repo,
                        &binary_root,
                        base_head.as_deref(),
                        conflicts,
                    )
                    .await;
            }

            self.commit_all_if_changed(
                &repo,
                &format!(
                    "sync: {} {}",
                    sanitize_commit_component(&request.device_name),
                    isoish_now()
                ),
            )
            .await?;
            if state.uses_remote() {
                self.fetch(&repo).await?;
                if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                    return self
                        .conflict_response(
                            &user,
                            &vault,
                            &repo,
                            &binary_root,
                            base_head.as_deref(),
                            conflicts,
                        )
                        .await;
                }
                if let Some(conflicts) = self.push_after_rebase(&repo, &state.branch).await? {
                    return self
                        .conflict_response(
                            &user,
                            &vault,
                            &repo,
                            &binary_root,
                            base_head.as_deref(),
                            conflicts,
                        )
                        .await;
                }
            }

            Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: self.head_from_repo(&repo).await?,
                files: self
                    .changed_files_since(
                        &repo,
                        &binary_root,
                        base_head.as_deref(),
                        &request.client_manifest,
                    )
                    .await?,
                conflicts: vec![],
            })
        })
        .await
    }

    pub async fn history(
        &self,
        user: &str,
        vault: &str,
        file_path: Option<&str>,
    ) -> Result<Vec<HistoryEntry>> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let repo = self.repo_dir(&user, &vault);
        if let Some(path) = file_path {
            let safe_path = validate_vault_path(path)?;
            if !is_text_or_code_path(&safe_path) {
                return self.binary_history(&repo, &safe_path).await;
            }
        }
        let mut args = vec![
            "log".to_string(),
            "--format=%H%x09%aI%x09%an%x09%s".to_string(),
        ];
        if let Some(path) = file_path {
            let safe_path = validate_vault_path(path)?;
            args.push("--".to_string());
            args.push(safe_path);
        }
        let output = String::from_utf8_lossy(&git_strings(&repo, &args, &[0]).await?.stdout)
            .trim()
            .to_string();
        Ok(parse_history_output(&output))
    }

    pub async fn activity_feed(&self, user: &str, limit: usize) -> Result<Vec<ActivityFeedEntry>> {
        let user = validate_slug(user, "user")?;
        let mut entries = Vec::new();
        let vaults_dir = self.data_dir.join("users").join(&user).join("vaults");
        let mut vaults = match fs::read_dir(vaults_dir).await {
            Ok(vaults) => vaults,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(error) => return Err(error.into()),
        };

        while let Some(entry) = vaults.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let vault = entry.file_name().to_string_lossy().to_string();
            if validate_slug(&vault, "vault").is_err() {
                continue;
            }
            let mut vault_entries = self
                .with_lock(&user, &vault, || async {
                    self.vault_activity(&user, &vault, limit).await
                })
                .await?;
            entries.append(&mut vault_entries);
        }

        entries.sort_by(|left, right| {
            right
                .date
                .cmp(&left.date)
                .then_with(|| right.hash.cmp(&left.hash))
        });
        entries.truncate(limit);
        Ok(entries)
    }

    async fn vault_activity(
        &self,
        user: &str,
        vault: &str,
        limit: usize,
    ) -> Result<Vec<ActivityFeedEntry>> {
        let repo = self.repo_dir(user, vault);
        if fs::metadata(repo.join(".git")).await.is_err() {
            return Ok(Vec::new());
        }
        let limit_arg = format!("-n{}", limit.clamp(1, 100));
        let output = git(
            Some(&repo),
            &["log", &limit_arg, "--format=%H%x09%aI%x09%an%x09%s"],
            &[0, 128],
        )
        .await?;
        if output.code != 0 {
            return Ok(Vec::new());
        }

        let history = parse_history_output(&String::from_utf8_lossy(&output.stdout));
        let mut entries = Vec::with_capacity(history.len());
        for entry in history {
            let files = self.changed_paths_for_commit(&repo, &entry.hash).await?;
            entries.push(ActivityFeedEntry {
                vault: vault.to_string(),
                hash: entry.hash,
                date: entry.date,
                author: entry.author,
                subject: entry.subject,
                files,
            });
        }
        Ok(entries)
    }

    async fn changed_paths_for_commit(&self, repo: &Path, commit: &str) -> Result<Vec<String>> {
        let commit = validate_commit_id(commit)?;
        let output = git(
            Some(repo),
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "--root",
                "-z",
                &commit,
            ],
            &[0],
        )
        .await?;
        let mut paths = BTreeSet::new();
        for path in split_nul(&output.stdout) {
            let path = validate_vault_path(&path)?;
            if path == BINARY_MANIFEST_PATH {
                for binary_path in self.changed_binary_paths_for_commit(repo, &commit).await? {
                    paths.insert(binary_path);
                }
            } else if is_client_visible_conflict_path(&path) {
                paths.insert(path);
            }
        }
        Ok(paths.into_iter().collect())
    }

    async fn changed_binary_paths_for_commit(
        &self,
        repo: &Path,
        commit: &str,
    ) -> Result<Vec<String>> {
        let current = self
            .binary_manifest_at(repo, commit)
            .await
            .unwrap_or_default();
        let parent = self
            .binary_manifest_at(repo, &format!("{}^", commit))
            .await
            .unwrap_or_default();
        let mut paths = BTreeSet::new();
        for (path, entry) in &current.files {
            if parent.files.get(path) != Some(entry) {
                paths.insert(path.clone());
            }
        }
        for path in parent.files.keys() {
            if !current.files.contains_key(path) {
                paths.insert(path.clone());
            }
        }
        Ok(paths.into_iter().collect())
    }

    async fn binary_history(&self, repo: &Path, path: &str) -> Result<Vec<HistoryEntry>> {
        let args = vec![
            "log".to_string(),
            "--format=%H%x09%aI%x09%an%x09%s".to_string(),
            "--".to_string(),
            BINARY_MANIFEST_PATH.to_string(),
        ];
        let output = String::from_utf8_lossy(&git_strings(repo, &args, &[0]).await?.stdout)
            .trim()
            .to_string();
        let mut entries = Vec::new();
        for entry in parse_history_output(&output) {
            let current = self
                .binary_manifest_at(repo, &entry.hash)
                .await
                .unwrap_or_default();
            let parent = self
                .binary_manifest_at(repo, &format!("{}^", entry.hash))
                .await
                .unwrap_or_default();
            let current_entry = current.files.get(path);
            let parent_entry = parent.files.get(path);
            if current_entry.is_some() && current_entry != parent_entry {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    pub async fn file_at_version(
        &self,
        user: &str,
        vault: &str,
        file_path: &str,
        hash: &str,
    ) -> Result<VersionFileResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let safe_path = validate_vault_path(file_path)?;
        let hash = validate_commit_id(hash)?;
        let repo = self.repo_dir(&user, &vault);
        let repo_str = path_to_str(&repo)?;
        let content = if is_text_or_code_path(&safe_path) {
            let spec = format!("{hash}:{safe_path}");
            git(None, &["-C", repo_str, "show", &spec], &[0])
                .await?
                .stdout
        } else {
            let spec = format!("{hash}:{BINARY_MANIFEST_PATH}");
            let manifest_bytes = git(None, &["-C", repo_str, "show", &spec], &[0])
                .await?
                .stdout;
            let manifest: BinaryManifest = serde_json::from_slice(&manifest_bytes)?;
            validate_manifest(&manifest)?;
            let entry = manifest
                .files
                .get(&safe_path)
                .ok_or_else(|| anyhow!("binary file not present at requested version"))?;
            read_binary_object(&self.binary_dir(&user, &vault), entry).await?
        };
        Ok(VersionFileResponse {
            path: safe_path,
            hash: hash.to_string(),
            sha256: sha256_hex(&content),
            content_base64: STANDARD.encode(&content),
            read_only: true,
        })
    }

    pub async fn resolve(
        &self,
        user: &str,
        vault: &str,
        request: ResolveRequest,
    ) -> Result<SyncResponse> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        self.with_lock(&user, &vault, || async {
            let state = self.read_state(&user, &vault).await?;
            self.validate_remote_url(&state.remote_url)?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            let upload_root = self.upload_dir(&user, &vault);
            let mut resolved_paths = Vec::new();
            for file in request.files {
                let safe = validate_vault_path(&file.path)?;
                let content = self
                    .content_from_inline_or_upload(
                        &upload_root,
                        &safe,
                        file.content_base64.as_ref(),
                        file.upload_id.as_ref(),
                    )
                    .await?;
                if is_text_or_code_path(&safe) {
                    write_repo_file(&repo, &safe, &content).await?;
                } else {
                    let mut manifest = read_manifest(&repo).await?;
                    let entry = store_binary(&binary_root, &safe, &content, 0).await?;
                    manifest.files.insert(safe.clone(), entry);
                    write_manifest(&repo, &manifest).await?;
                }
                resolved_paths.push(safe);
            }
            self.configure_git(&repo, &state).await?;
            self.commit_all_if_changed(
                &repo,
                &format!(
                    "sync: resolve {} {}",
                    sanitize_commit_component(&request.device_name),
                    isoish_now()
                ),
            )
            .await?;
            if state.uses_remote() {
                self.fetch(&repo).await?;
                if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                    return self
                        .conflict_response(&user, &vault, &repo, &binary_root, None, conflicts)
                        .await;
                }
                if let Some(conflicts) = self.push_after_rebase(&repo, &state.branch).await? {
                    return self
                        .conflict_response(&user, &vault, &repo, &binary_root, None, conflicts)
                        .await;
                }
            }
            self.clear_pending_conflicts(&user, &vault, &resolved_paths)
                .await?;
            Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: self.head_from_repo(&repo).await?,
                files: self
                    .changed_files_since(&repo, &binary_root, None, &[])
                    .await?,
                conflicts: vec![],
            })
        })
        .await
    }

    async fn ensure_repo(&self, state: &VaultState) -> Result<()> {
        self.validate_remote_url(&state.remote_url)?;
        validate_git_branch(&state.branch)?;
        let vault_dir = self.vault_dir(&state.user, &state.vault);
        let repo = self.repo_dir(&state.user, &state.vault);
        fs::create_dir_all(&vault_dir).await?;
        fs::create_dir_all(self.binary_dir(&state.user, &state.vault)).await?;
        if fs::metadata(repo.join(".git")).await.is_err() {
            if state.uses_remote() {
                let _ = fs::remove_dir_all(&repo).await;
                git(
                    Some(&vault_dir),
                    &["clone", &state.remote_url, "repo"],
                    &[0],
                )
                .await?;
            } else {
                fs::create_dir_all(&repo).await?;
                git(
                    Some(&vault_dir),
                    &["init", "-b", &state.branch, "repo"],
                    &[0],
                )
                .await?;
            }
        }
        self.configure_git(&repo, state).await?;
        if state.uses_remote() {
            self.ensure_remote_branch(&repo, &state.branch).await
        } else {
            self.ensure_local_branch(&repo, &state.branch).await
        }
    }

    async fn ensure_remote_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        let branch = validate_git_branch(branch)?;
        let remote_branch = format!("origin/{branch}");
        self.fetch(repo).await?;
        let remote = git(
            Some(repo),
            &["rev-parse", "--verify", &remote_branch],
            &[0, 128],
        )
        .await?;
        if remote.code == 0 {
            git(
                Some(repo),
                &["checkout", "-B", branch.as_str(), &remote_branch],
                &[0],
            )
            .await?;
        } else {
            git(Some(repo), &["checkout", "-B", branch.as_str()], &[0]).await?;
        }
        Ok(())
    }

    async fn ensure_local_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        let branch = validate_git_branch(branch)?;
        let branch_ref = format!("refs/heads/{branch}");
        let local = git(
            Some(repo),
            &["rev-parse", "--verify", &branch_ref],
            &[0, 128],
        )
        .await?;
        if local.code == 0 {
            git(Some(repo), &["checkout", branch.as_str()], &[0]).await?;
            return Ok(());
        }

        let head = git(Some(repo), &["rev-parse", "--verify", "HEAD"], &[0, 128]).await?;
        if head.code == 0 {
            git(Some(repo), &["checkout", "-b", branch.as_str()], &[0]).await?;
        } else {
            let branch_ref = format!("refs/heads/{branch}");
            git(Some(repo), &["symbolic-ref", "HEAD", &branch_ref], &[0]).await?;
        }
        Ok(())
    }

    fn validate_remote_url(&self, remote_url: &str) -> Result<()> {
        if remote_url.trim().is_empty() {
            Ok(())
        } else {
            self.remote_policy.validate(remote_url)
        }
    }

    async fn apply_client_changes(
        &self,
        user: &str,
        vault: &str,
        repo: &Path,
        binary_root: &Path,
        upload_root: &Path,
        base_head: Option<&str>,
        changes: &[ClientChange],
    ) -> Result<Vec<SyncConflict>> {
        let mut conflicts = Vec::new();
        let pending_conflicts = self.read_pending_conflicts(user, vault).await?;
        let mut manifest = read_manifest(repo).await?;
        let base_manifest = match base_head {
            Some(head) => self
                .binary_manifest_at(repo, head)
                .await
                .unwrap_or_default(),
            None => BinaryManifest::default(),
        };
        let mut binary_manifest_changed = false;

        for change in changes {
            match change {
                ClientChange::Delete { path } => {
                    let safe = validate_vault_path(path)?;
                    if pending_conflicts.contains(&safe) {
                        conflicts.push(SyncConflict {
                            path: safe,
                            reason: PENDING_CONFLICT_REASON.to_string(),
                        });
                        continue;
                    }
                    if is_text_or_code_path(&safe) {
                        if let Some(conflict) = apply_text_delete(repo, base_head, &safe).await? {
                            conflicts.push(conflict);
                        }
                    } else {
                        let current_entry = manifest.files.get(&safe);
                        let base_entry = base_manifest.files.get(&safe);
                        if base_head.is_some() && current_entry != base_entry {
                            conflicts.push(SyncConflict {
                                path: safe,
                                reason:
                                    "binary file changed on the server while the client deleted it"
                                        .to_string(),
                            });
                            continue;
                        }
                        manifest.files.remove(&safe);
                        binary_manifest_changed = true;
                    }
                }
                ClientChange::Upsert {
                    path,
                    content_base64,
                    upload_id,
                    mtime,
                    ..
                } => {
                    let safe = validate_vault_path(path)?;
                    if pending_conflicts.contains(&safe) {
                        conflicts.push(SyncConflict {
                            path: safe,
                            reason: PENDING_CONFLICT_REASON.to_string(),
                        });
                        continue;
                    }
                    let content = self
                        .content_from_inline_or_upload(
                            upload_root,
                            &safe,
                            content_base64.as_ref(),
                            upload_id.as_ref(),
                        )
                        .await?;
                    if is_text_or_code_path(&safe) {
                        if let Some(conflict) =
                            apply_text_upsert(repo, base_head, &safe, &content).await?
                        {
                            conflicts.push(conflict);
                        }
                    } else {
                        let current_entry = manifest.files.get(&safe);
                        let base_entry = base_manifest.files.get(&safe);
                        if base_head.is_some() && current_entry != base_entry {
                            conflicts.push(SyncConflict {
                                path: safe,
                                reason: "binary file changed on both server and client; choose one version and resolve from Obsidian".to_string(),
                            });
                            continue;
                        }
                        let entry =
                            store_binary(binary_root, &safe, &content, mtime.unwrap_or(0)).await?;
                        manifest.files.insert(safe, entry);
                        binary_manifest_changed = true;
                    }
                }
            }
        }

        if binary_manifest_changed {
            write_manifest(repo, &manifest).await?;
        }
        Ok(conflicts)
    }

    async fn changed_files_since(
        &self,
        repo: &Path,
        binary_root: &Path,
        base_head: Option<&str>,
        client_manifest: &[ManifestEntry],
    ) -> Result<Vec<ServerFileChange>> {
        let mut files = Vec::new();
        let client_manifest_by_path: HashMap<&str, &ManifestEntry> = client_manifest
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let text_paths = if let Some(base) = base_head {
            if self.valid_commit(repo, base).await? {
                split_nul(
                    &git(
                        Some(repo),
                        &["diff", "--name-only", "-z", base, "HEAD", "--"],
                        &[0],
                    )
                    .await?
                    .stdout,
                )
            } else {
                split_nul(&git(Some(repo), &["ls-files", "-z"], &[0]).await?.stdout)
            }
        } else {
            split_nul(&git(Some(repo), &["ls-files", "-z"], &[0]).await?.stdout)
        };

        for path in text_paths {
            if path == BINARY_MANIFEST_PATH {
                continue;
            }
            let path = validate_vault_path(&path)?;
            if !is_text_or_code_path(&path) {
                continue;
            }
            let absolute = repo_path(repo, &path)?;
            match fs::read(&absolute).await {
                Ok(content) => {
                    let sha256 = sha256_hex(&content);
                    if base_head.is_none()
                        && client_has_manifest_entry(
                            &client_manifest_by_path,
                            &path,
                            &sha256,
                            content.len() as u64,
                        )
                    {
                        continue;
                    }
                    files.push(ServerFileChange::Upsert {
                        path,
                        sha256,
                        content_base64: STANDARD.encode(content),
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    files.push(ServerFileChange::Delete { path })
                }
                Err(error) => return Err(error.into()),
            }
        }

        let current_manifest = read_manifest(repo).await?;
        let previous_manifest = if let Some(base) = base_head {
            self.binary_manifest_at(repo, base)
                .await
                .unwrap_or_default()
        } else {
            BinaryManifest::default()
        };
        for (path, entry) in current_manifest.files.iter() {
            if previous_manifest.files.get(path) != Some(entry) {
                if base_head.is_none()
                    && client_has_manifest_entry(
                        &client_manifest_by_path,
                        path,
                        &entry.sha256,
                        entry.size,
                    )
                {
                    continue;
                }
                let content = read_binary_object(binary_root, entry).await?;
                files.push(ServerFileChange::Upsert {
                    path: path.clone(),
                    sha256: entry.sha256.clone(),
                    content_base64: STANDARD.encode(content),
                });
            }
        }
        for path in previous_manifest.files.keys() {
            if !current_manifest.files.contains_key(path) {
                files.push(ServerFileChange::Delete { path: path.clone() });
            }
        }

        files.sort_by(|left, right| file_path(left).cmp(file_path(right)));
        Ok(files)
    }

    async fn binary_manifest_at(&self, repo: &Path, commit: &str) -> Result<BinaryManifest> {
        let spec = format!("{commit}:{BINARY_MANIFEST_PATH}");
        let result = git(None, &["-C", path_to_str(repo)?, "show", &spec], &[0, 128]).await?;
        if result.code == 0 {
            let manifest: BinaryManifest = serde_json::from_slice(&result.stdout)?;
            validate_manifest(&manifest)?;
            Ok(manifest)
        } else {
            Ok(BinaryManifest::default())
        }
    }

    async fn fetch(&self, repo: &Path) -> Result<()> {
        git(Some(repo), &["fetch", "origin"], &[0])
            .await
            .map(|_| ())
    }

    async fn rebase_remote(&self, repo: &Path, branch: &str) -> Result<Option<Vec<SyncConflict>>> {
        let branch = validate_git_branch(branch)?;
        let remote_branch = format!("origin/{branch}");
        let remote = git(
            Some(repo),
            &["rev-parse", "--verify", &remote_branch],
            &[0, 128],
        )
        .await?;
        if remote.code != 0 {
            return Ok(None);
        }
        let result = git(Some(repo), &["rebase", &remote_branch], &[0, 1]).await?;
        if result.code == 0 {
            return Ok(None);
        }
        let conflicts = conflict_files(repo).await?;
        Ok(Some(
            conflicts
                .into_iter()
                .map(|path| SyncConflict {
                    path,
                    reason: "git rebase reported content conflicts".to_string(),
                })
                .collect(),
        ))
    }

    async fn push_after_rebase(
        &self,
        repo: &Path,
        branch: &str,
    ) -> Result<Option<Vec<SyncConflict>>> {
        let branch = validate_git_branch(branch)?;
        let head = git(Some(repo), &["rev-parse", "--verify", "HEAD"], &[0, 128]).await?;
        if head.code != 0 {
            return Ok(None);
        }

        let first_push = git(
            Some(repo),
            &["push", "-u", "origin", branch.as_str()],
            &[0, 1],
        )
        .await?;
        if first_push.code == 0 {
            return Ok(None);
        }

        self.fetch(repo).await?;
        if let Some(conflicts) = self.rebase_remote(repo, &branch).await? {
            return Ok(Some(conflicts));
        }

        git(Some(repo), &["push", "-u", "origin", branch.as_str()], &[0]).await?;
        Ok(None)
    }

    async fn commit_all_if_changed(&self, repo: &Path, message: &str) -> Result<()> {
        let status = git(Some(repo), &["status", "--porcelain", "-z"], &[0]).await?;
        if status.stdout.is_empty() {
            return Ok(());
        }
        git(Some(repo), &["add", "-A"], &[0]).await?;
        git(Some(repo), &["commit", "-m", message], &[0]).await?;
        Ok(())
    }

    async fn configure_git(&self, repo: &Path, state: &VaultState) -> Result<()> {
        let author_name = validate_git_identity(&state.author_name, "author name")?;
        let author_email = validate_git_identity(&state.author_email, "author email")?;
        git(Some(repo), &["config", "user.name", &author_name], &[0]).await?;
        git(Some(repo), &["config", "user.email", &author_email], &[0]).await?;
        Ok(())
    }

    async fn valid_commit(&self, repo: &Path, commit: &str) -> Result<bool> {
        Ok(git(
            Some(repo),
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            &[0, 1, 128],
        )
        .await?
        .code
            == 0)
    }

    async fn conflict_response(
        &self,
        user: &str,
        vault: &str,
        repo: &Path,
        binary_root: &Path,
        base_head: Option<&str>,
        conflicts: Vec<SyncConflict>,
    ) -> Result<SyncResponse> {
        self.record_pending_conflicts(user, vault, &conflicts)
            .await?;
        let conflict_paths: Vec<String> = conflicts
            .iter()
            .filter(|conflict| is_client_visible_conflict_path(&conflict.path))
            .filter(|conflict| is_text_or_code_path(&conflict.path))
            .filter(|conflict| conflict.reason != PENDING_CONFLICT_REASON)
            .map(|conflict| conflict.path.clone())
            .collect();
        let files = match self
            .files_for_paths(repo, binary_root, &conflict_paths)
            .await
        {
            Ok(files) => files,
            Err(_) => {
                self.changed_files_since(repo, binary_root, base_head, &[])
                    .await?
            }
        };
        self.cleanup_conflict_state(repo).await?;
        let server_head = self.head_from_repo(repo).await?;
        Ok(SyncResponse {
            status: SyncStatus::Conflict,
            server_head,
            files,
            conflicts,
        })
    }

    async fn cleanup_conflict_state(&self, repo: &Path) -> Result<()> {
        let git_dir = repo.join(".git");
        if fs::metadata(git_dir.join("rebase-merge")).await.is_ok()
            || fs::metadata(git_dir.join("rebase-apply")).await.is_ok()
        {
            git(Some(repo), &["rebase", "--abort"], &[0, 128]).await?;
            return Ok(());
        }

        git(Some(repo), &["reset", "--hard", "HEAD"], &[0, 128]).await?;
        git(Some(repo), &["clean", "-fd"], &[0]).await?;
        Ok(())
    }

    async fn files_for_paths(
        &self,
        repo: &Path,
        binary_root: &Path,
        paths: &[String],
    ) -> Result<Vec<ServerFileChange>> {
        let manifest = read_manifest(repo).await?;
        let mut files = Vec::new();
        for path in paths {
            if is_text_or_code_path(path) {
                let absolute = repo_path(repo, path)?;
                match fs::read(absolute).await {
                    Ok(content) => files.push(ServerFileChange::Upsert {
                        path: path.clone(),
                        sha256: sha256_hex(&content),
                        content_base64: STANDARD.encode(content),
                    }),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        files.push(ServerFileChange::Delete { path: path.clone() })
                    }
                    Err(error) => return Err(error.into()),
                }
            } else if let Some(entry) = manifest.files.get(path) {
                files.push(ServerFileChange::Upsert {
                    path: path.clone(),
                    sha256: entry.sha256.clone(),
                    content_base64: STANDARD.encode(read_binary_object(binary_root, entry).await?),
                });
            }
        }
        Ok(files)
    }

    async fn head_from_repo(&self, repo: &Path) -> Result<Option<String>> {
        let result = git(Some(repo), &["rev-parse", "--verify", "HEAD"], &[0, 128]).await?;
        if result.code == 0 {
            Ok(Some(
                String::from_utf8_lossy(&result.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }

    async fn read_state(&self, user: &str, vault: &str) -> Result<VaultState> {
        Ok(serde_json::from_slice(
            &fs::read(self.vault_dir(user, vault).join("state.json")).await?,
        )?)
    }

    async fn write_state(&self, state: &VaultState) -> Result<()> {
        fs::write(
            self.vault_dir(&state.user, &state.vault).join("state.json"),
            serde_json::to_vec_pretty(state)?,
        )
        .await?;
        Ok(())
    }

    fn vault_dir(&self, user: &str, vault: &str) -> PathBuf {
        self.data_dir
            .join("users")
            .join(user)
            .join("vaults")
            .join(vault)
    }

    fn repo_dir(&self, user: &str, vault: &str) -> PathBuf {
        self.vault_dir(user, vault).join("repo")
    }

    fn binary_dir(&self, user: &str, vault: &str) -> PathBuf {
        self.vault_dir(user, vault).join("binary")
    }

    fn upload_dir(&self, user: &str, vault: &str) -> PathBuf {
        self.vault_dir(user, vault).join("uploads")
    }

    fn upload_state_path(&self, user: &str, vault: &str, upload_id: &str) -> PathBuf {
        self.upload_dir(user, vault)
            .join(format!("{upload_id}.json"))
    }

    fn upload_content_path(&self, user: &str, vault: &str, upload_id: &str) -> PathBuf {
        self.upload_dir(user, vault)
            .join(format!("{upload_id}.bin"))
    }

    fn pending_conflicts_path(&self, user: &str, vault: &str) -> PathBuf {
        self.vault_dir(user, vault).join(PENDING_CONFLICTS_PATH)
    }

    async fn read_pending_conflicts(&self, user: &str, vault: &str) -> Result<BTreeSet<String>> {
        let path = self.pending_conflicts_path(user, vault);
        let bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeSet::new());
            }
            Err(error) => return Err(error.into()),
        };
        let paths: Vec<String> = serde_json::from_slice(&bytes)?;
        paths
            .into_iter()
            .map(|path| validate_vault_path(&path))
            .collect()
    }

    async fn write_pending_conflicts(
        &self,
        user: &str,
        vault: &str,
        paths: &BTreeSet<String>,
    ) -> Result<()> {
        fs::write(
            self.pending_conflicts_path(user, vault),
            serde_json::to_vec_pretty(&paths.iter().collect::<Vec<_>>())?,
        )
        .await?;
        Ok(())
    }

    async fn record_pending_conflicts(
        &self,
        user: &str,
        vault: &str,
        conflicts: &[SyncConflict],
    ) -> Result<()> {
        let mut pending = self.read_pending_conflicts(user, vault).await?;
        for conflict in conflicts {
            let path = validate_vault_path(&conflict.path)?;
            if is_client_visible_conflict_path(&path) {
                pending.insert(path);
            }
        }
        self.write_pending_conflicts(user, vault, &pending).await
    }

    async fn clear_pending_conflicts(
        &self,
        user: &str,
        vault: &str,
        resolved_paths: &[String],
    ) -> Result<()> {
        let mut pending = self.read_pending_conflicts(user, vault).await?;
        for path in resolved_paths {
            pending.remove(path);
        }
        if pending.is_empty() {
            let _ = fs::remove_file(self.pending_conflicts_path(user, vault)).await;
            return Ok(());
        }
        self.write_pending_conflicts(user, vault, &pending).await
    }

    async fn new_upload_id(&self, upload_dir: &Path) -> Result<String> {
        for _ in 0..8 {
            let upload_id = random_upload_id()?;
            if fs::metadata(upload_dir.join(format!("{upload_id}.json")))
                .await
                .is_err()
            {
                return Ok(upload_id);
            }
        }
        bail!("could not allocate upload id")
    }

    async fn read_upload_state(
        &self,
        user: &str,
        vault: &str,
        upload_id: &str,
    ) -> Result<UploadState> {
        let state: UploadState = serde_json::from_slice(
            &fs::read(self.upload_state_path(user, vault, upload_id)).await?,
        )?;
        validate_vault_path(&state.path)?;
        validate_sha256_hex(&state.sha256)?;
        if state.received > state.size || state.size > i64::MAX as u64 {
            bail!("invalid upload state");
        }
        Ok(state)
    }

    async fn write_upload_state(
        &self,
        user: &str,
        vault: &str,
        upload_id: &str,
        state: &UploadState,
    ) -> Result<()> {
        validate_vault_path(&state.path)?;
        validate_sha256_hex(&state.sha256)?;
        fs::write(
            self.upload_state_path(user, vault, upload_id),
            serde_json::to_vec_pretty(state)?,
        )
        .await?;
        Ok(())
    }

    async fn content_from_inline_or_upload(
        &self,
        upload_root: &Path,
        path: &str,
        content_base64: Option<&String>,
        upload_id: Option<&String>,
    ) -> Result<Vec<u8>> {
        match (content_base64, upload_id) {
            (Some(content_base64), None) => Ok(STANDARD.decode(content_base64.as_bytes())?),
            (None, Some(upload_id)) => read_upload_content(upload_root, path, upload_id).await,
            _ => bail!("upsert must include exactly one content source"),
        }
    }

    async fn with_lock<F, Fut, T>(&self, user: &str, vault: &str, operation: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let key = format!("{user}/{vault}");
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        operation().await
    }
}

fn validate_sha256_hex(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid upload checksum");
    }
    Ok(value)
}

fn validate_upload_id(value: &str) -> Result<String> {
    let value = value.trim();
    if value.len() != 32 || !value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid upload id");
    }
    Ok(value.to_string())
}

fn random_upload_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("could not generate upload id: {error}"))?;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn client_has_manifest_entry(
    client_manifest_by_path: &HashMap<&str, &ManifestEntry>,
    path: &str,
    sha256: &str,
    size: u64,
) -> bool {
    client_manifest_by_path
        .get(path)
        .map(|entry| entry.sha256 == sha256 && entry.size == size)
        .unwrap_or(false)
}

async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn read_upload_content(
    upload_root: &Path,
    expected_path: &str,
    upload_id: &str,
) -> Result<Vec<u8>> {
    let upload_id = validate_upload_id(upload_id)?;
    let state_path = upload_root.join(format!("{upload_id}.json"));
    let content_path = upload_root.join(format!("{upload_id}.bin"));
    let state: UploadState = serde_json::from_slice(&fs::read(&state_path).await?)?;
    if !state.complete {
        bail!("upload is not complete");
    }
    if validate_vault_path(&state.path)? != expected_path {
        bail!("upload path mismatch");
    }
    let expected_sha = validate_sha256_hex(&state.sha256)?;
    let content = fs::read(&content_path).await?;
    if content.len() as u64 != state.size || sha256_hex(&content) != expected_sha {
        bail!("upload checksum mismatch");
    }
    let _ = fs::remove_file(&state_path).await;
    let _ = fs::remove_file(&content_path).await;
    Ok(content)
}

async fn apply_text_upsert(
    repo: &Path,
    base_head: Option<&str>,
    path: &str,
    client_content: &[u8],
) -> Result<Option<SyncConflict>> {
    let absolute = repo_path(repo, path)?;
    let current = fs::read(&absolute).await.ok();
    let base = match base_head {
        Some(head) => read_file_at_commit(repo, head, path).await?,
        None => None,
    };

    match (base, current) {
        (Some(base), Some(current)) => {
            if current.as_slice() == client_content || client_content == base.as_slice() {
                return Ok(None);
            }
            if current == base {
                write_repo_file(repo, path, client_content).await?;
                return Ok(None);
            }
            merge_text_file(repo, path, &base, &current, client_content).await
        }
        (Some(base), None) => {
            if client_content == base.as_slice() {
                Ok(None)
            } else {
                write_conflict(
                    repo,
                    path,
                    None,
                    Some(client_content),
                    "server deleted file while client edited it",
                )
                .await
            }
        }
        (None, Some(current)) => {
            if current.as_slice() == client_content {
                Ok(None)
            } else {
                write_conflict(
                    repo,
                    path,
                    Some(&current),
                    Some(client_content),
                    "server and client changed file without a shared base",
                )
                .await
            }
        }
        (None, None) => {
            write_repo_file(repo, path, client_content).await?;
            Ok(None)
        }
    }
}

async fn apply_text_delete(
    repo: &Path,
    base_head: Option<&str>,
    path: &str,
) -> Result<Option<SyncConflict>> {
    let absolute = repo_path(repo, path)?;
    let current = fs::read(&absolute).await.ok();
    let Some(current) = current else {
        return Ok(None);
    };
    let base = match base_head {
        Some(head) => read_file_at_commit(repo, head, path).await?,
        None => None,
    };
    if base.as_ref() == Some(&current) || base.is_none() {
        let _ = fs::remove_file(absolute).await;
        return Ok(None);
    }
    write_conflict(
        repo,
        path,
        Some(&current),
        None,
        "client deleted file while server edited it",
    )
    .await
}

async fn merge_text_file(
    repo: &Path,
    path: &str,
    base: &[u8],
    current: &[u8],
    client: &[u8],
) -> Result<Option<SyncConflict>> {
    if base.contains(&0) || current.contains(&0) || client.contains(&0) {
        return Ok(Some(SyncConflict {
            path: path.to_string(),
            reason: "binary-like text file changed on both server and client".to_string(),
        }));
    }
    let temp_root = std::env::temp_dir().join(format!(
        "obsidian-git-sync-{}",
        sha256_hex(&[current, base, client].concat())
    ));
    let _ = fs::remove_dir_all(&temp_root).await;
    fs::create_dir_all(&temp_root).await?;
    let server_path = temp_root.join("server");
    let base_path = temp_root.join("base");
    let client_path = temp_root.join("client");
    fs::write(&server_path, current).await?;
    fs::write(&base_path, base).await?;
    fs::write(&client_path, client).await?;
    let server_path = path_to_str(&server_path)?;
    let base_path = path_to_str(&base_path)?;
    let client_path = path_to_str(&client_path)?;
    let output = git(
        None,
        &[
            "merge-file",
            "-p",
            "-L",
            "server",
            "-L",
            "base",
            "-L",
            "client",
            server_path,
            base_path,
            client_path,
        ],
        &[0, 1],
    )
    .await;
    let _ = fs::remove_dir_all(&temp_root).await;
    if output.is_err() {
        return write_conflict(
            repo,
            path,
            Some(current),
            Some(client),
            "git merge-file could not merge files",
        )
        .await;
    }
    let output = output?;
    write_repo_file(repo, path, &output.stdout).await?;
    if output.code == 0 {
        Ok(None)
    } else {
        Ok(Some(SyncConflict {
            path: path.to_string(),
            reason: "git merge-file reported content conflicts".to_string(),
        }))
    }
}

async fn write_conflict(
    repo: &Path,
    path: &str,
    server: Option<&[u8]>,
    client: Option<&[u8]>,
    reason: &str,
) -> Result<Option<SyncConflict>> {
    let server_text = server
        .map(String::from_utf8_lossy)
        .map(|cow| cow.to_string())
        .unwrap_or_else(|| "[deleted]".to_string());
    let client_text = client
        .map(String::from_utf8_lossy)
        .map(|cow| cow.to_string())
        .unwrap_or_else(|| "[deleted]".to_string());
    let content = format!(
        "<<<<<<< server\n{}\n=======\n{}\n>>>>>>> client\n",
        server_text.trim_end(),
        client_text.trim_end()
    );
    write_repo_file(repo, path, content.as_bytes()).await?;
    Ok(Some(SyncConflict {
        path: path.to_string(),
        reason: reason.to_string(),
    }))
}

async fn write_repo_file(repo: &Path, path: &str, content: &[u8]) -> Result<()> {
    let absolute = repo_path(repo, path)?;
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(absolute, content).await?;
    Ok(())
}

async fn read_file_at_commit(repo: &Path, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!("{commit}:{path}");
    let result = git(None, &["-C", path_to_str(repo)?, "show", &spec], &[0, 128]).await?;
    if result.code == 0 {
        Ok(Some(result.stdout))
    } else {
        Ok(None)
    }
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("filesystem path is not utf8"))
}

async fn conflict_files(repo: &Path) -> Result<Vec<String>> {
    split_nul(
        &git(
            Some(repo),
            &["diff", "--name-only", "--diff-filter=U", "-z"],
            &[0],
        )
        .await?
        .stdout,
    )
    .into_iter()
    .map(|path| validate_vault_path(&path))
    .collect()
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

fn file_path(file: &ServerFileChange) -> &str {
    match file {
        ServerFileChange::Upsert { path, .. } => path,
        ServerFileChange::Delete { path } => path,
    }
}

fn parse_history_output(output: &str) -> Vec<HistoryEntry> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            HistoryEntry {
                hash: parts.next().unwrap_or_default().to_string(),
                date: parts.next().unwrap_or_default().to_string(),
                author: parts.next().unwrap_or_default().to_string(),
                subject: parts.collect::<Vec<_>>().join("\t"),
            }
        })
        .collect()
}

fn validate_optional_commit_id(input: Option<&str>) -> Result<Option<String>> {
    input.map(validate_commit_id).transpose()
}

fn is_client_visible_conflict_path(path: &str) -> bool {
    path != BINARY_MANIFEST_PATH && !path.starts_with(".obsidian-git-sync/")
}

fn isoish_now() -> String {
    // Avoid pulling a time crate into the server just for commit subjects.
    format!("{:?}", std::time::SystemTime::now())
}
