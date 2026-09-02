//! Per-device WebDAV passwords.
//!
//! A device password is a server-generated secret that grants a single external device (for
//! example an e-ink tablet) WebDAV access to exactly one folder of one vault. The plugin creates
//! and revokes them with the user's normal bearer session; the device only ever sees the
//! generated password, never the user's login.

use crate::auth::normalize_user_claim;
use crate::paths::{sanitize_commit_component, validate_slug, validate_vault_path};
use crate::time_format::{rfc3339_from_unix, unix_now};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::Mutex;

/// Unambiguous lowercase alphabet (no 0/o, 1/l/i) so the password can be typed on a tablet.
const PASSWORD_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const PASSWORD_GROUPS: usize = 5;
const PASSWORD_GROUP_LENGTH: usize = 4;
const LAST_USED_WRITE_INTERVAL_SECONDS: u64 = 300;
const MAX_DEVICE_PASSWORDS_PER_USER: usize = 50;
const MAX_LABEL_LENGTH: usize = 80;

#[derive(Debug)]
pub struct DevicePasswordStore {
    store_path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoreFile {
    #[serde(default)]
    passwords: Vec<DevicePasswordRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DevicePasswordRecord {
    id: String,
    user: String,
    vault: String,
    folder: String,
    label: String,
    password_hash: String,
    created_at: u64,
    #[serde(default)]
    last_used_at: Option<u64>,
}

/// Public view of a device password. Never carries the secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DevicePasswordEntry {
    pub id: String,
    pub label: String,
    pub vault: String,
    pub folder: String,
    pub username: String,
    pub webdav_path: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreateDevicePasswordRequest {
    pub label: String,
    pub folder: String,
}

/// Returned exactly once, right after creation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreatedDevicePassword {
    #[serde(flatten)]
    pub entry: DevicePasswordEntry,
    pub password: String,
}

/// What a successfully authenticated WebDAV request is allowed to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceGrant {
    pub id: String,
    pub user: String,
    pub vault: String,
    pub folder: String,
    pub label: String,
}

impl DevicePasswordStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            store_path: data_dir.into().join("auth/device-passwords.json"),
            lock: Mutex::new(()),
        }
    }

    pub async fn create(
        &self,
        user: &str,
        vault: &str,
        request: CreateDevicePasswordRequest,
    ) -> Result<CreatedDevicePassword> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let folder = validate_device_folder(&request.folder)?;
        let label = validate_label(&request.label)?;

        let _guard = self.lock.lock().await;
        let mut store = self.read_store().await?;
        if store
            .passwords
            .iter()
            .filter(|record| record.user == user)
            .count()
            >= MAX_DEVICE_PASSWORDS_PER_USER
        {
            bail!("invalid request: too many device passwords for this user");
        }
        let password = generate_password()?;
        let record = DevicePasswordRecord {
            id: random_id()?,
            user,
            vault,
            folder,
            label,
            password_hash: hash_password(&password),
            created_at: unix_now(),
            last_used_at: None,
        };
        let entry = record.public_entry();
        store.passwords.push(record);
        self.write_store(&store).await?;
        Ok(CreatedDevicePassword { entry, password })
    }

    pub async fn list(&self, user: &str, vault: &str) -> Result<Vec<DevicePasswordEntry>> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let store = self.read_store().await?;
        Ok(store
            .passwords
            .iter()
            .filter(|record| record.user == user && record.vault == vault)
            .map(DevicePasswordRecord::public_entry)
            .collect())
    }

    /// Removes the password. Returns `false` when no password with that id belongs to the user.
    pub async fn revoke(&self, user: &str, vault: &str, id: &str) -> Result<bool> {
        let user = validate_slug(user, "user")?;
        let vault = validate_slug(vault, "vault")?;
        let _guard = self.lock.lock().await;
        let mut store = self.read_store().await?;
        let before = store.passwords.len();
        store
            .passwords
            .retain(|record| !(record.user == user && record.vault == vault && record.id == id));
        if store.passwords.len() == before {
            return Ok(false);
        }
        self.write_store(&store).await?;
        Ok(true)
    }

    /// Verifies a WebDAV username/password pair. The username is the user's namespace; the
    /// password identifies the device. Records last use at most every few minutes.
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<DeviceGrant> {
        let user = normalize_user_claim(username).map_err(|_| anyhow!("unauthorized"))?;
        let password_hash = hash_password(password.trim());
        let _guard = self.lock.lock().await;
        let mut store = self.read_store().await?;
        let now = unix_now();
        let record = store
            .passwords
            .iter_mut()
            .find(|record| {
                record.user == user && constant_time_eq(&record.password_hash, &password_hash)
            })
            .ok_or_else(|| anyhow!("unauthorized"))?;
        let grant = DeviceGrant {
            id: record.id.clone(),
            user: record.user.clone(),
            vault: record.vault.clone(),
            folder: record.folder.clone(),
            label: record.label.clone(),
        };
        let should_record = record
            .last_used_at
            .map(|last| now.saturating_sub(last) >= LAST_USED_WRITE_INTERVAL_SECONDS)
            .unwrap_or(true);
        if should_record {
            record.last_used_at = Some(now);
            self.write_store(&store).await?;
        }
        Ok(grant)
    }

    async fn read_store(&self) -> Result<StoreFile> {
        match fs::read(&self.store_path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_store(&self, store: &StoreFile) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temp_path = temp_store_path(&self.store_path);
        fs::write(&temp_path, serde_json::to_vec_pretty(store)?).await?;
        fs::rename(temp_path, &self.store_path).await?;
        Ok(())
    }
}

impl DevicePasswordRecord {
    fn public_entry(&self) -> DevicePasswordEntry {
        DevicePasswordEntry {
            id: self.id.clone(),
            label: self.label.clone(),
            vault: self.vault.clone(),
            folder: self.folder.clone(),
            username: self.user.clone(),
            webdav_path: webdav_path(&self.vault, &self.folder),
            created_at: rfc3339_from_unix(self.created_at),
            last_used_at: self.last_used_at.map(rfc3339_from_unix),
        }
    }
}

pub fn webdav_path(vault: &str, folder: &str) -> String {
    let mut path = format!("/dav/{}", encode_path_segment(vault));
    for segment in folder.split('/').filter(|segment| !segment.is_empty()) {
        path.push('/');
        path.push_str(&encode_path_segment(segment));
    }
    path.push('/');
    path
}

pub fn encode_path_segment(segment: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(segment, SEGMENT).to_string()
}

/// Folders are vault-relative, never the vault root, and never the server's own metadata folder.
pub fn validate_device_folder(input: &str) -> Result<String> {
    let trimmed = input
        .trim()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string();
    if trimmed.is_empty() {
        bail!("invalid device password folder: choose a folder inside the vault");
    }
    if trimmed.len() > 512 || trimmed.chars().any(char::is_control) {
        bail!("invalid device password folder");
    }
    let folder = validate_vault_path(&trimmed)
        .map_err(|_| anyhow!("invalid device password folder: {trimmed}"))?;
    if folder == ".obsidian-git-sync" || folder.starts_with(".obsidian-git-sync/") {
        bail!("invalid device password folder: {folder} is reserved");
    }
    Ok(folder)
}

fn validate_label(input: &str) -> Result<String> {
    let label = sanitize_commit_component(input);
    if input.trim().is_empty() {
        bail!("invalid device password label: enter a name for the device");
    }
    if label.len() > MAX_LABEL_LENGTH {
        bail!("invalid device password label: too long");
    }
    Ok(label)
}

fn generate_password() -> Result<String> {
    let mut bytes = [0_u8; PASSWORD_GROUPS * PASSWORD_GROUP_LENGTH];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("random generator failed: {error}"))?;
    let mut groups = Vec::with_capacity(PASSWORD_GROUPS);
    for group in bytes.chunks(PASSWORD_GROUP_LENGTH) {
        groups.push(
            group
                .iter()
                .map(|byte| PASSWORD_ALPHABET[(*byte as usize) % PASSWORD_ALPHABET.len()] as char)
                .collect::<String>(),
        );
    }
    Ok(groups.join("-"))
}

fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("random generator failed: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hash_password(password: &str) -> String {
    // Device passwords are server-generated with ~100 bits of entropy, so a plain SHA-256 is
    // sufficient and keeps per-request WebDAV authentication cheap.
    let digest = Sha256::digest(password.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = *left.get(index).unwrap_or(&0);
        let right_byte = *right.get(index).unwrap_or(&0);
        diff |= (left_byte ^ right_byte) as usize;
    }
    diff == 0
}

fn temp_store_path(path: &Path) -> PathBuf {
    let mut temp_path = path.to_path_buf();
    temp_path.set_extension("json.tmp");
    temp_path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_typeable() {
        let password = generate_password().unwrap();
        assert_eq!(
            password.len(),
            PASSWORD_GROUPS * (PASSWORD_GROUP_LENGTH + 1) - 1
        );
        assert!(password
            .split('-')
            .all(|group| group.len() == PASSWORD_GROUP_LENGTH
                && group.bytes().all(|byte| PASSWORD_ALPHABET.contains(&byte))));
        assert_ne!(password, generate_password().unwrap());
    }

    #[test]
    fn validates_folders() {
        assert_eq!(
            validate_device_folder(" /Tablet/Notes/ ").unwrap(),
            "Tablet/Notes"
        );
        assert!(validate_device_folder("").is_err());
        assert!(validate_device_folder("../escape").is_err());
        assert!(validate_device_folder(".git/hooks").is_err());
        assert!(validate_device_folder(".obsidian-git-sync/x").is_err());
    }

    #[test]
    fn builds_encoded_webdav_paths() {
        assert_eq!(
            webdav_path("notes", "E-Ink/My Notes"),
            "/dav/notes/E-Ink/My%20Notes/"
        );
    }

    #[tokio::test]
    async fn creates_authenticates_and_revokes() {
        let root = tempfile::tempdir().unwrap();
        let store = DevicePasswordStore::new(root.path());
        let created = store
            .create(
                "alice",
                "notes",
                CreateDevicePasswordRequest {
                    label: "Boox tablet".to_string(),
                    folder: "Tablet".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.entry.username, "alice");
        assert_eq!(created.entry.webdav_path, "/dav/notes/Tablet/");

        let grant = store
            .authenticate("Alice", &created.password)
            .await
            .unwrap();
        assert_eq!(grant.vault, "notes");
        assert_eq!(grant.folder, "Tablet");
        assert_eq!(grant.label, "Boox tablet");
        assert!(store.authenticate("alice", "wrong").await.is_err());
        assert!(store.authenticate("bob", &created.password).await.is_err());

        let listed = store.list("alice", "notes").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].last_used_at.is_some());
        assert!(store.list("alice", "other").await.unwrap().is_empty());

        assert!(store
            .revoke("alice", "notes", &created.entry.id)
            .await
            .unwrap());
        assert!(!store
            .revoke("alice", "notes", &created.entry.id)
            .await
            .unwrap());
        assert!(store
            .authenticate("alice", &created.password)
            .await
            .is_err());
    }
}
