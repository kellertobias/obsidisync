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
use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

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
            self.remote_policy.validate(&request.remote_url)?;
            let branch = validate_git_branch(&request.branch)?;
            let state = VaultState {
                user: user.clone(),
                vault: vault.clone(),
                remote_url: request.remote_url.trim().to_string(),
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
            self.remote_policy.validate(&state.remote_url)?;
            let base_head = validate_optional_commit_id(request.base_head.as_deref())?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            self.configure_git(&repo, &state).await?;
            self.fetch(&repo).await?;
            self.commit_all_if_changed(&repo, "sync: server pending")
                .await?;
            if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                return self
                    .conflict_response(&repo, &binary_root, base_head.as_deref(), conflicts)
                    .await;
            }

            let conflicts = self
                .apply_client_changes(&repo, &binary_root, base_head.as_deref(), &request.changes)
                .await?;
            if !conflicts.is_empty() {
                return self
                    .conflict_response(&repo, &binary_root, base_head.as_deref(), conflicts)
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
            self.fetch(&repo).await?;
            if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                return self
                    .conflict_response(&repo, &binary_root, base_head.as_deref(), conflicts)
                    .await;
            }
            if let Some(conflicts) = self.push_after_rebase(&repo, &state.branch).await? {
                return self
                    .conflict_response(&repo, &binary_root, base_head.as_deref(), conflicts)
                    .await;
            }

            Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: self.head_from_repo(&repo).await?,
                files: self
                    .changed_files_since(&repo, &binary_root, base_head.as_deref())
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
            self.remote_policy.validate(&state.remote_url)?;
            let repo = self.repo_dir(&user, &vault);
            let binary_root = self.binary_dir(&user, &vault);
            for file in request.files {
                let safe = validate_vault_path(&file.path)?;
                let content = STANDARD.decode(file.content_base64.as_bytes())?;
                if is_text_or_code_path(&safe) {
                    write_repo_file(&repo, &safe, &content).await?;
                } else {
                    let mut manifest = read_manifest(&repo).await?;
                    let entry = store_binary(&binary_root, &safe, &content, 0).await?;
                    manifest.files.insert(safe, entry);
                    write_manifest(&repo, &manifest).await?;
                }
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
            self.fetch(&repo).await?;
            if let Some(conflicts) = self.rebase_remote(&repo, &state.branch).await? {
                return self
                    .conflict_response(&repo, &binary_root, None, conflicts)
                    .await;
            }
            if let Some(conflicts) = self.push_after_rebase(&repo, &state.branch).await? {
                return self
                    .conflict_response(&repo, &binary_root, None, conflicts)
                    .await;
            }
            Ok(SyncResponse {
                status: SyncStatus::Ok,
                server_head: self.head_from_repo(&repo).await?,
                files: self.changed_files_since(&repo, &binary_root, None).await?,
                conflicts: vec![],
            })
        })
        .await
    }

    async fn ensure_repo(&self, state: &VaultState) -> Result<()> {
        self.remote_policy.validate(&state.remote_url)?;
        validate_git_branch(&state.branch)?;
        let vault_dir = self.vault_dir(&state.user, &state.vault);
        let repo = self.repo_dir(&state.user, &state.vault);
        fs::create_dir_all(&vault_dir).await?;
        fs::create_dir_all(self.binary_dir(&state.user, &state.vault)).await?;
        if fs::metadata(repo.join(".git")).await.is_err() {
            let _ = fs::remove_dir_all(&repo).await;
            git(
                Some(&vault_dir),
                &["clone", &state.remote_url, "repo"],
                &[0],
            )
            .await?;
        }
        self.configure_git(&repo, state).await?;
        self.ensure_branch(&repo, &state.branch).await
    }

    async fn ensure_branch(&self, repo: &Path, branch: &str) -> Result<()> {
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

    async fn apply_client_changes(
        &self,
        repo: &Path,
        binary_root: &Path,
        base_head: Option<&str>,
        changes: &[ClientChange],
    ) -> Result<Vec<SyncConflict>> {
        let mut conflicts = Vec::new();
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
                    mtime,
                    ..
                } => {
                    let safe = validate_vault_path(path)?;
                    let content = STANDARD.decode(content_base64.as_bytes())?;
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
    ) -> Result<Vec<ServerFileChange>> {
        let mut files = Vec::new();
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
                Ok(content) => files.push(ServerFileChange::Upsert {
                    path,
                    sha256: sha256_hex(&content),
                    content_base64: STANDARD.encode(content),
                }),
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
        repo: &Path,
        binary_root: &Path,
        base_head: Option<&str>,
        conflicts: Vec<SyncConflict>,
    ) -> Result<SyncResponse> {
        let conflict_paths: Vec<String> = conflicts
            .iter()
            .filter(|conflict| is_client_visible_conflict_path(&conflict.path))
            .filter(|conflict| is_text_or_code_path(&conflict.path))
            .map(|conflict| conflict.path.clone())
            .collect();
        let files = match self
            .files_for_paths(repo, binary_root, &conflict_paths)
            .await
        {
            Ok(files) => files,
            Err(_) => {
                self.changed_files_since(repo, binary_root, base_head)
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
