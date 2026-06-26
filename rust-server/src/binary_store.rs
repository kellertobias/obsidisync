use crate::paths::{repo_path, validate_vault_path};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::fs;

pub const BINARY_MANIFEST_PATH: &str = ".obsidian-git-sync/binary-manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct BinaryManifest {
    pub files: BTreeMap<String, BinaryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BinaryEntry {
    pub sha256: String,
    pub mtime: i64,
    pub size: u64,
    pub object_path: String,
}

pub fn sha256_hex(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

pub async fn read_manifest(repo: &Path) -> Result<BinaryManifest> {
    let path = repo_path(repo, BINARY_MANIFEST_PATH)?;
    match fs::read(&path).await {
        Ok(bytes) => {
            let manifest: BinaryManifest = serde_json::from_slice(&bytes)?;
            validate_manifest(&manifest)?;
            Ok(manifest)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BinaryManifest::default()),
        Err(error) => Err(error.into()),
    }
}

pub async fn write_manifest(repo: &Path, manifest: &BinaryManifest) -> Result<()> {
    validate_manifest(manifest)?;
    let path = repo_path(repo, BINARY_MANIFEST_PATH)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(manifest)?).await?;
    Ok(())
}

pub async fn store_binary(
    binary_root: &Path,
    path: &str,
    content: &[u8],
    mtime: i64,
) -> Result<BinaryEntry> {
    validate_vault_path(path)?;
    let sha = sha256_hex(content);
    let object_path = format!("{}/{}", &sha[0..2], sha);
    let absolute = binary_root.join(&object_path);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).await?;
    }
    if fs::metadata(&absolute).await.is_err() {
        fs::write(&absolute, content).await?;
    }
    Ok(BinaryEntry {
        sha256: sha,
        mtime,
        size: content.len() as u64,
        object_path,
    })
}

pub async fn read_binary_object(binary_root: &Path, entry: &BinaryEntry) -> Result<Vec<u8>> {
    validate_binary_entry("binary object", entry)?;
    Ok(fs::read(binary_root.join(&entry.object_path)).await?)
}

pub fn validate_manifest(manifest: &BinaryManifest) -> Result<()> {
    for (path, entry) in &manifest.files {
        validate_vault_path(path)?;
        validate_binary_entry(path, entry)?;
    }
    Ok(())
}

fn validate_binary_entry(path: &str, entry: &BinaryEntry) -> Result<()> {
    if !is_sha256_hex(&entry.sha256) {
        bail!("invalid binary manifest entry for {path}");
    }
    let expected_object_path = format!("{}/{}", &entry.sha256[0..2], entry.sha256);
    if entry.object_path != expected_object_path || entry.size > i64::MAX as u64 {
        bail!("invalid binary manifest entry for {path}");
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_manifest_object_path_traversal() {
        let mut manifest = BinaryManifest::default();
        manifest.files.insert(
            "Images/photo.png".to_string(),
            BinaryEntry {
                sha256: "a".repeat(64),
                mtime: 0,
                size: 1,
                object_path: "../../secret".to_string(),
            },
        );
        assert!(validate_manifest(&manifest).is_err());
    }
}
