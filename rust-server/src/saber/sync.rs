//! Turns Saber's encrypted uploads into PDFs inside the vault.
//!
//! Every WebDAV write by a Saber device queues the touched paths here. After a short debounce
//! (Saber uploads a note's preview, assets and main file as separate PUTs) the renderer reads
//! the note and its assets back from the vault, decrypts them with the user's Saber encryption
//! password, renders a PDF and writes it to the configured PDF folder — as the same device, so
//! the PDF shows up in Obsidian on the next sync like any other WebDAV upload. Deleted notes
//! (Saber uploads an empty file) remove their PDF again.

use super::crypto::{SaberCipher, CONFIG_FILE_NAME};
use super::pdf::{self, Assets};
use super::sbn::Note;
use crate::device_passwords::DeviceGrant;
use crate::vault::VaultService;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Extension of Saber's current note format.
pub const NOTE_EXTENSION: &str = ".sbn2";
/// Saber's original JSON note format; not supported by the renderer.
const OLD_NOTE_EXTENSION: &str = ".sbn";
/// How long to wait after the last upload before rendering, so a note and its assets are
/// rendered once, together.
pub const DEFAULT_RENDER_DELAY: Duration = Duration::from_millis(1500);
/// Upper bound on the number of asset files looked up per note.
const MAX_ASSETS: usize = 500;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RenderReport {
    /// Vault paths of PDFs that were written (or refreshed).
    pub rendered: Vec<String>,
    /// Vault paths of PDFs removed because their note was deleted.
    pub deleted: Vec<String>,
    /// Notes that could not be rendered, with the reason.
    pub failed: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct SaberRenderer {
    vaults: VaultService,
    delay: Duration,
    pending: Mutex<HashMap<String, PendingGrant>>,
    in_flight: AtomicUsize,
    idle: Notify,
}

#[derive(Debug)]
struct PendingGrant {
    grant: DeviceGrant,
    paths: BTreeSet<String>,
}

impl SaberRenderer {
    pub fn new(vaults: VaultService, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            vaults,
            delay,
            pending: Mutex::new(HashMap::new()),
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
        })
    }

    /// Queues paths changed by a Saber device and renders them after the debounce delay.
    pub fn schedule(self: &Arc<Self>, grant: &DeviceGrant, paths: Vec<String>) {
        let renderer = Arc::clone(self);
        let grant = grant.clone();
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            {
                let mut pending = renderer.pending.lock().await;
                pending
                    .entry(grant.id.clone())
                    .or_insert_with(|| PendingGrant {
                        grant: grant.clone(),
                        paths: BTreeSet::new(),
                    })
                    .paths
                    .extend(paths);
            }
            if !renderer.delay.is_zero() {
                tokio::time::sleep(renderer.delay).await;
            }
            let batch = renderer.pending.lock().await.remove(&grant.id);
            if let Some(batch) = batch {
                let paths: Vec<String> = batch.paths.into_iter().collect();
                match renderer.render_paths(&batch.grant, &paths).await {
                    Ok(report) => {
                        if !report.failed.is_empty() {
                            for (note, reason) in &report.failed {
                                tracing::warn!(device = %batch.grant.label, note = %note, reason = %reason, "saber note could not be rendered");
                            }
                        }
                        if !report.rendered.is_empty() || !report.deleted.is_empty() {
                            tracing::info!(device = %batch.grant.label, rendered = report.rendered.len(), deleted = report.deleted.len(), "saber PDFs updated");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(device = %batch.grant.label, error = %error, "saber rendering failed");
                    }
                }
            }
            if renderer.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
                renderer.idle.notify_waiters();
            }
        });
    }

    /// Resolves once no scheduled render is pending or running. Used by tests.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Renders (or removes) the PDFs for every note touched by `changed_paths`.
    pub async fn render_paths(
        &self,
        grant: &DeviceGrant,
        changed_paths: &[String],
    ) -> Result<RenderReport> {
        let settings = grant
            .saber_rendering()
            .ok_or_else(|| anyhow!("device {} has no Saber encryption password", grant.label))?;
        let folder = grant.folder.as_str();
        let prefix = format!("{folder}/");

        let file_names: Vec<&str> = changed_paths
            .iter()
            .filter_map(|path| path.strip_prefix(&prefix))
            .filter(|name| !name.contains('/'))
            .collect();
        if file_names.is_empty() {
            return Ok(RenderReport::default());
        }

        let cipher = match self
            .load_cipher(grant, &settings.encryption_password)
            .await?
        {
            Some(cipher) => cipher,
            None => {
                tracing::info!(device = %grant.label, "saber config.sbc not uploaded yet; skipping render");
                return Ok(RenderReport::default());
            }
        };

        let mut notes = BTreeSet::new();
        for name in file_names {
            if name == CONFIG_FILE_NAME {
                continue;
            }
            let Some(note_path) = cipher.decrypt_file_name(name) else {
                continue;
            };
            match classify(&note_path) {
                Some(base) => {
                    notes.insert(base);
                }
                None if note_path.ends_with(OLD_NOTE_EXTENSION) => {
                    tracing::warn!(device = %grant.label, note = %note_path, "saber .sbn (legacy JSON) notes are not rendered; open and re-save the note in Saber");
                }
                None => {}
            }
        }

        let mut report = RenderReport::default();
        for note_path in notes {
            match self
                .render_note(grant, &settings.pdf_folder, &cipher, &note_path)
                .await
            {
                Ok(Some(NoteOutcome::Rendered(pdf_path))) => report.rendered.push(pdf_path),
                Ok(Some(NoteOutcome::Deleted(pdf_path))) => report.deleted.push(pdf_path),
                Ok(None) => {}
                Err(error) => report.failed.push((note_path, format!("{error:#}"))),
            }
        }
        Ok(report)
    }

    async fn load_cipher(
        &self,
        grant: &DeviceGrant,
        password: &str,
    ) -> Result<Option<SaberCipher>> {
        let config_path = format!("{}/{CONFIG_FILE_NAME}", grant.folder);
        let Some(config) = self.read_optional(grant, &config_path).await? else {
            return Ok(None);
        };
        SaberCipher::from_config(password, &config)
            .map(Some)
            .context("saber encryption password check failed")
    }

    async fn render_note(
        &self,
        grant: &DeviceGrant,
        pdf_folder: &str,
        cipher: &SaberCipher,
        note_path: &str,
    ) -> Result<Option<NoteOutcome>> {
        let pdf_path = pdf_path_for(pdf_folder, note_path)
            .ok_or_else(|| anyhow!("note path {note_path} cannot be mapped into the vault"))?;
        let encrypted_path = format!("{}/{}", grant.folder, cipher.encrypt_file_name(note_path));
        let device = grant.dav_device();

        let encrypted = self.read_optional(grant, &encrypted_path).await?;
        let Some(encrypted) = encrypted.filter(|bytes| !bytes.is_empty()) else {
            // Deleted in Saber (or never uploaded): drop the PDF if we made one.
            if self
                .vaults
                .dav_stat(&grant.user, &grant.vault, &pdf_path)
                .await?
                .is_some()
            {
                self.vaults
                    .dav_delete(&grant.user, &grant.vault, &pdf_path, &device)
                    .await?;
                return Ok(Some(NoteOutcome::Deleted(pdf_path)));
            }
            return Ok(None);
        };

        let bytes = cipher.decrypt(&encrypted).context("decrypting note")?;
        let note = Note::parse(&bytes).context("parsing note")?;

        let mut assets = Assets::new();
        for index in note.asset_indices().into_iter().take(MAX_ASSETS) {
            let asset_note_path = format!("{note_path}.{index}");
            let asset_path = format!(
                "{}/{}",
                grant.folder,
                cipher.encrypt_file_name(&asset_note_path)
            );
            if let Some(encrypted_asset) = self.read_optional(grant, &asset_path).await? {
                if encrypted_asset.is_empty() {
                    continue;
                }
                match cipher.decrypt(&encrypted_asset) {
                    Ok(asset) => {
                        assets.insert(index, asset);
                    }
                    Err(error) => {
                        tracing::warn!(note = %note_path, index, error = %error, "saber asset could not be decrypted");
                    }
                }
            }
        }

        let pdf = tokio::task::spawn_blocking(move || pdf::render(&note, &assets))
            .await
            .map_err(|error| anyhow!("render task failed: {error}"))?
            .context("rendering PDF")?;
        self.vaults
            .dav_write(&grant.user, &grant.vault, &pdf_path, pdf, &device)
            .await
            .context("writing PDF into the vault")?;
        Ok(Some(NoteOutcome::Rendered(pdf_path)))
    }

    async fn read_optional(&self, grant: &DeviceGrant, path: &str) -> Result<Option<Vec<u8>>> {
        match self
            .vaults
            .dav_stat(&grant.user, &grant.vault, path)
            .await?
        {
            Some(entry) if !entry.is_dir => {
                let (_, content) = self
                    .vaults
                    .dav_read(&grant.user, &grant.vault, path)
                    .await?;
                Ok(Some(content))
            }
            _ => Ok(None),
        }
    }
}

enum NoteOutcome {
    Rendered(String),
    Deleted(String),
}

/// Maps any Saber file path (`/a/b.sbn2`, `/a/b.sbn2.3` asset, `/a/b.sbn2.p` preview) to the
/// note it belongs to. Returns `None` for files that are not `.sbn2` notes.
pub fn classify(note_path: &str) -> Option<String> {
    if let Some(base) = note_path.strip_suffix(NOTE_EXTENSION) {
        return Some(format!("{base}{NOTE_EXTENSION}"));
    }
    let (base, suffix) = note_path.rsplit_once('.')?;
    if !base.ends_with(NOTE_EXTENSION) {
        return None;
    }
    if suffix == "p" || suffix.chars().all(|ch| ch.is_ascii_digit()) && !suffix.is_empty() {
        return Some(base.to_string());
    }
    None
}

/// Vault path of the PDF for a note, mirroring Saber's folder tree below the PDF folder.
pub fn pdf_path_for(pdf_folder: &str, note_path: &str) -> Option<String> {
    let base = note_path.strip_suffix(NOTE_EXTENSION)?;
    let relative = base.trim_start_matches('/');
    let candidate = format!("{pdf_folder}/{relative}.pdf");
    crate::vault::dav::validate_dav_path(&candidate).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_saber_paths() {
        assert_eq!(classify("/a/b.sbn2").as_deref(), Some("/a/b.sbn2"));
        assert_eq!(classify("/a/b.sbn2.3").as_deref(), Some("/a/b.sbn2"));
        assert_eq!(classify("/a/b.sbn2.p").as_deref(), Some("/a/b.sbn2"));
        assert_eq!(classify("/a/b.sbn"), None);
        assert_eq!(classify("/a/b.sbn2.x"), None);
        assert_eq!(classify("/Readme.md"), None);
    }

    #[test]
    fn maps_notes_to_pdf_paths() {
        assert_eq!(
            pdf_path_for("Saber", "/Uni/Lecture 1.sbn2").as_deref(),
            Some("Saber/Uni/Lecture 1.pdf")
        );
        assert_eq!(pdf_path_for("Saber", "/../x.sbn2"), None);
        assert_eq!(pdf_path_for("Saber", "/x.sbn"), None);
    }
}
