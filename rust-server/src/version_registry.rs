use crate::protocol::DeviceEntry;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

pub const DEVICES_FILE_NAME: &str = "devices.json";
pub const VERSION_METADATA_FILE_NAME: &str = "version-metadata.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistry {
    pub devices: HashMap<String, DeviceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadataStore {
    pub entries: HashMap<String, VersionMetadataEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadataEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub squashed_into_hash: Option<String>,
}

pub fn version_metadata_key(source_path: &str, hash: &str) -> String {
    format!("{source_path}|{hash}")
}

// Both registries are plain, un-versioned server-local state - same tier as `state.json` and
// `pending-conflicts.json` (see `VaultService::vault_dir`). They intentionally live outside the
// git repo: committing them would create sync-adjacent commits that touch no vault file, which
// pollutes the per-file `git log` and the vault-wide activity feed (a device sync or a squash
// would otherwise look like a change to whatever commit happened to be HEAD).

pub async fn read_devices(path: &Path) -> Result<DeviceRegistry> {
    match fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DeviceRegistry::default()),
        Err(error) => Err(error.into()),
    }
}

pub async fn write_devices(path: &Path, registry: &DeviceRegistry) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(registry)?).await?;
    Ok(())
}

pub async fn read_version_metadata(path: &Path) -> Result<VersionMetadataStore> {
    match fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(VersionMetadataStore::default())
        }
        Err(error) => Err(error.into()),
    }
}

pub async fn write_version_metadata(path: &Path, store: &VersionMetadataStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(store)?).await?;
    Ok(())
}
