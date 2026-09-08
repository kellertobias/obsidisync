//! Pushes PDFs from the vault into Saber.
//!
//! When a markdown note carries the `#tablet` tag (in its body or in frontmatter `tags`), or
//! lives inside a Saber device's PDF folder (`Tablet/` by default, so a note like
//! `Tablet/Tablet.md` can simply list what belongs on the tablet), every PDF it links or embeds
//! is turned into a Saber note: one page per PDF page with the PDF as the
//! page background, exactly what Saber's own "import PDF" produces. The note and its single PDF
//! asset are encrypted with the device's key and written into the Saber sync folder, mirroring
//! the vault path (`Uni/Slides.pdf` becomes `Uni/Slides.sbn2` in Saber). Saber downloads them on
//! its next sync like anything another Saber device uploaded.
//!
//! Each PDF is pushed once. A changed PDF is left alone so strokes already drawn in Saber keep
//! their meaning; removing the tag and adding it again pushes the current file afresh.

use super::crypto::SaberCipher;
use super::sync::load_cipher;
use crate::device_passwords::{DeviceGrant, DevicePasswordStore};
use crate::vault::VaultService;
use anyhow::{anyhow, bail, Context, Result};
use bson::{doc, Bson, Document};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// The Obsidian tag that marks a note whose PDFs go to the tablet.
pub const TAG: &str = "tablet";
pub const DEFAULT_PUSH_DELAY: Duration = Duration::from_millis(1500);
/// Saber's `EditorPage.defaultWidth`; imported PDF pages are scaled to this width.
const PAGE_WIDTH: f64 = 1000.0;
const STATE_FILE: &str = "saber-push.json";
const MAX_PDF_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushReport {
    /// Vault paths of PDFs pushed to Saber (per device, so a PDF can appear more than once).
    pub pushed: Vec<String>,
    pub skipped_unchanged: usize,
    pub failed: Vec<(String, String)>,
}

/// Persisted per vault: which tagged notes reference which PDFs, and which PDFs were pushed.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PushState {
    /// Tagged markdown notes and the PDF paths they referenced at the last scan.
    #[serde(default)]
    notes: BTreeMap<String, Vec<String>>,
    /// PDF path to the sha256 (per device id) that was pushed.
    #[serde(default)]
    pushed: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug)]
pub struct TabletPusher {
    vaults: VaultService,
    device_passwords: Arc<DevicePasswordStore>,
    delay: Duration,
    pending: Mutex<BTreeMap<(String, String), BTreeSet<String>>>,
    fully_scanned: Mutex<HashSet<(String, String)>>,
    state_lock: Mutex<()>,
    in_flight: AtomicUsize,
    idle: Notify,
}

impl TabletPusher {
    pub fn new(
        vaults: VaultService,
        device_passwords: Arc<DevicePasswordStore>,
        delay: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            vaults,
            device_passwords,
            delay,
            pending: Mutex::new(BTreeMap::new()),
            fully_scanned: Mutex::new(HashSet::new()),
            state_lock: Mutex::new(()),
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
        })
    }

    /// Queues vault paths changed by a sync. Only markdown notes matter; other paths are
    /// ignored cheaply here so callers can pass everything.
    pub fn schedule(self: &Arc<Self>, user: &str, vault: &str, paths: Vec<String>) {
        let notes: Vec<String> = paths.into_iter().filter(|path| is_markdown(path)).collect();
        let pusher = Arc::clone(self);
        let key = (user.to_string(), vault.to_string());
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            pusher
                .pending
                .lock()
                .await
                .entry(key.clone())
                .or_default()
                .extend(notes);
            if !pusher.delay.is_zero() {
                tokio::time::sleep(pusher.delay).await;
            }
            let batch = pusher.pending.lock().await.remove(&key);
            if let Some(batch) = batch {
                let notes: Vec<String> = batch.into_iter().collect();
                match Box::pin(pusher.run(&key.0, &key.1, &notes)).await {
                    Ok(report) => {
                        for (path, reason) in &report.failed {
                            tracing::warn!(vault = %key.1, pdf = %path, reason = %reason, "tablet push failed");
                        }
                        if !report.pushed.is_empty() {
                            tracing::info!(vault = %key.1, pushed = report.pushed.len(), "PDFs pushed to Saber");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(vault = %key.1, error = %format!("{error:#}"), "tablet push run failed");
                    }
                }
            }
            if pusher.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
                pusher.idle.notify_waiters();
            }
        });
    }

    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Scans the given notes (or the whole vault the first time) and pushes tagged PDFs to
    /// every Saber device of the vault.
    pub async fn run(
        &self,
        user: &str,
        vault: &str,
        changed_notes: &[String],
    ) -> Result<PushReport> {
        let grants = self.device_passwords.saber_grants(user, vault).await?;
        if grants.is_empty() {
            return Ok(PushReport::default());
        }
        let all_paths = self.vaults.list_tracked_paths(user, vault).await?;
        // PDF folders hold Saber's own rendered output; never push those back.
        let list_folders: Vec<String> = grants
            .iter()
            .filter_map(|grant| grant.saber.as_ref().map(|saber| saber.pdf_folder.clone()))
            .collect();
        let pdfs: Vec<String> = all_paths
            .iter()
            .filter(|path| is_pdf(path) && !under_any(path, &list_folders))
            .cloned()
            .collect();

        let first_scan = self
            .fully_scanned
            .lock()
            .await
            .insert((user.to_string(), vault.to_string()));
        let notes: Vec<String> = if first_scan {
            all_paths
                .iter()
                .filter(|path| is_markdown(path))
                .cloned()
                .collect()
        } else {
            changed_notes.to_vec()
        };

        let _guard = self.state_lock.lock().await;
        let state_path = self
            .vaults
            .data_dir
            .join("users")
            .join(user)
            .join("vaults")
            .join(vault)
            .join(STATE_FILE);
        let mut state = read_state(&state_path).await?;
        let mut report = PushReport::default();

        for note in &notes {
            let content = match self.read_optional(user, vault, note).await? {
                Some(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                None => String::new(),
            };
            let tagged =
                !content.is_empty() && (has_tag(&content, TAG) || under_any(note, &list_folders));
            if !tagged {
                if let Some(previous) = state.notes.remove(note) {
                    // The note lost its tag (or was deleted): forget its PDFs unless another
                    // tagged note still references them, so re-tagging pushes them again.
                    let still_referenced: HashSet<&String> =
                        state.notes.values().flatten().collect();
                    for pdf in previous {
                        if !still_referenced.contains(&pdf) {
                            state.pushed.remove(&pdf);
                        }
                    }
                }
                continue;
            }
            let referenced = resolve_pdf_links(&content, note, &pdfs);
            state.notes.insert(note.to_string(), referenced.clone());
            for pdf in referenced {
                match Box::pin(self.push_pdf(user, vault, &grants, &pdf, &mut state)).await {
                    Ok(outcome) => match outcome {
                        PushOutcome::Pushed(devices) => {
                            report
                                .pushed
                                .extend(std::iter::repeat_n(pdf.clone(), devices));
                        }
                        PushOutcome::Unchanged => report.skipped_unchanged += 1,
                    },
                    Err(error) => report.failed.push((pdf, format!("{error:#}"))),
                }
            }
        }

        write_state(&state_path, &state).await?;
        Ok(report)
    }

    async fn push_pdf(
        &self,
        user: &str,
        vault: &str,
        grants: &[DeviceGrant],
        pdf: &str,
        state: &mut PushState,
    ) -> Result<PushOutcome> {
        let entry = self
            .vaults
            .dav_stat(user, vault, pdf)
            .await?
            .ok_or_else(|| anyhow!("PDF is missing from the vault"))?;
        if entry.size > MAX_PDF_BYTES {
            bail!("PDF is larger than {MAX_PDF_BYTES} bytes");
        }
        let sha256 = entry.etag.clone();
        let pushed_for = state.pushed.entry(pdf.to_string()).or_default();
        let targets: Vec<&DeviceGrant> = grants
            .iter()
            .filter(|grant| !pushed_for.contains_key(&grant.id))
            .collect();
        if targets.is_empty() {
            return Ok(PushOutcome::Unchanged);
        }

        let (_, bytes) = self.vaults.dav_read(user, vault, pdf).await?;
        // PDF parsing is CPU work with deep frames; keep it off the async worker stack.
        let (bytes, note_bytes) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Vec<u8>)> {
                let pages = pdf_page_sizes(&bytes).context("reading PDF pages")?;
                let note = build_pdf_note(&pages);
                let mut note_bytes = Vec::new();
                note.to_writer(&mut note_bytes)?;
                Ok((bytes, note_bytes))
            })
            .await
            .map_err(|error| anyhow!("PDF task failed: {error}"))??;
        let saber_path = saber_note_path(pdf);

        let mut count = 0;
        for grant in targets {
            let settings = grant
                .saber_rendering()
                .ok_or_else(|| anyhow!("device {} has no encryption password", grant.label))?;
            let Some(cipher) =
                load_cipher(&self.vaults, grant, &settings.encryption_password).await?
            else {
                tracing::info!(device = %grant.label, "saber config.sbc not uploaded yet; PDF push postponed");
                continue;
            };
            self.write_encrypted(grant, &cipher, &format!("{saber_path}.0"), &bytes)
                .await?;
            self.write_encrypted(grant, &cipher, &saber_path, &note_bytes)
                .await?;
            pushed_for.insert(grant.id.clone(), sha256.clone());
            count += 1;
        }
        Ok(if count == 0 {
            PushOutcome::Unchanged
        } else {
            PushOutcome::Pushed(count)
        })
    }

    async fn write_encrypted(
        &self,
        grant: &DeviceGrant,
        cipher: &SaberCipher,
        saber_path: &str,
        plaintext: &[u8],
    ) -> Result<()> {
        let vault_path = format!("{}/{}", grant.folder, cipher.encrypt_file_name(saber_path));
        self.vaults
            .dav_write(
                &grant.user,
                &grant.vault,
                &vault_path,
                cipher.encrypt(plaintext),
                &grant.dav_device(),
            )
            .await
            .with_context(|| format!("writing {saber_path} for {}", grant.label))?;
        Ok(())
    }

    async fn read_optional(&self, user: &str, vault: &str, path: &str) -> Result<Option<Vec<u8>>> {
        match self.vaults.dav_stat(user, vault, path).await? {
            Some(entry) if !entry.is_dir => {
                Ok(Some(self.vaults.dav_read(user, vault, path).await?.1))
            }
            _ => Ok(None),
        }
    }
}

enum PushOutcome {
    Pushed(usize),
    Unchanged,
}

async fn read_state(path: &std::path::Path) -> Result<PushState> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PushState::default()),
        Err(error) => Err(error.into()),
    }
}

async fn write_state(path: &std::path::Path, state: &PushState) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = path.with_extension("json.tmp");
    tokio::fs::write(&temp, serde_json::to_vec_pretty(state)?).await?;
    tokio::fs::rename(temp, path).await?;
    Ok(())
}

fn under_any(path: &str, folders: &[String]) -> bool {
    folders
        .iter()
        .any(|folder| path.starts_with(&format!("{folder}/")))
}

fn is_markdown(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".md")
}

fn is_pdf(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".pdf")
}

/// The Saber-side note path for a vault PDF: same folders, `.sbn2` instead of `.pdf`.
pub fn saber_note_path(pdf_path: &str) -> String {
    let stem = &pdf_path[..pdf_path.len() - 4];
    format!("/{stem}.sbn2")
}

/// Whether the note carries `#tag` (or a nested `#tag/...`) in its body or lists `tag` in its
/// frontmatter `tags`. Matching is case-insensitive, like Obsidian's.
pub fn has_tag(markdown: &str, tag: &str) -> bool {
    let tag_lower = tag.to_ascii_lowercase();
    if frontmatter_tags(markdown)
        .iter()
        .any(|found| found == &tag_lower || found.starts_with(&format!("{tag_lower}/")))
    {
        return true;
    }
    let body = body_without_frontmatter(markdown);
    let lower = body.to_ascii_lowercase();
    let needle = format!("#{tag_lower}");
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower[from..].find(&needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = start == 0
            || matches!(
                bytes[start - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b',' | b';'
            );
        let after_ok = end == bytes.len()
            || bytes[end] == b'/'
            || !(bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-'));
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

fn body_without_frontmatter(markdown: &str) -> &str {
    let Some(rest) = markdown.strip_prefix("---") else {
        return markdown;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return markdown;
    };
    for terminator in ["\n---\n", "\n---\r\n", "\n...\n"] {
        if let Some(index) = rest.find(terminator) {
            return &rest[index + terminator.len()..];
        }
    }
    if rest.ends_with("\n---") {
        return "";
    }
    markdown
}

/// Tags from a YAML frontmatter `tags` (or `tag`) key: inline lists, comma-separated values,
/// and block lists. Values are lowercased and stripped of a leading `#`.
pub fn frontmatter_tags(markdown: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let Some(rest) = markdown.strip_prefix("---") else {
        return tags;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return tags;
    };
    let end = rest
        .find("\n---")
        .or_else(|| rest.find("\n..."))
        .unwrap_or(rest.len());
    let frontmatter = &rest[..end];
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let key = key.trim().to_ascii_lowercase();
        if key != "tags" && key != "tag" {
            continue;
        }
        let value = value.trim();
        if value.is_empty() {
            while let Some(next) = lines.peek() {
                let item = next.trim();
                if let Some(item) = item.strip_prefix("- ").or_else(|| item.strip_prefix('-')) {
                    push_tag(&mut tags, item);
                    lines.next();
                } else {
                    break;
                }
            }
        } else {
            let inner = value
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .unwrap_or(value);
            for item in inner.split(',') {
                push_tag(&mut tags, item);
            }
        }
    }
    tags
}

fn push_tag(tags: &mut Vec<String>, raw: &str) {
    let cleaned = raw
        .trim()
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .trim_start_matches('#')
        .to_ascii_lowercase();
    if !cleaned.is_empty() {
        tags.push(cleaned);
    }
}

/// Vault paths of PDFs the note links or embeds, resolved the way Obsidian resolves links:
/// exact vault path, path relative to the note's folder, or a unique file name anywhere.
pub fn resolve_pdf_links(markdown: &str, note_path: &str, vault_pdfs: &[String]) -> Vec<String> {
    let note_dir = note_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let mut found = Vec::new();
    for target in link_targets(markdown) {
        if let Some(resolved) = resolve_target(&target, note_dir, vault_pdfs) {
            if !found.contains(&resolved) {
                found.push(resolved);
            }
        }
    }
    found
}

/// Raw link targets ending in `.pdf` from `[[...]]`, `![[...]]`, and `[text](...)` links.
fn link_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = markdown;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let target = inner.split(['|', '#']).next().unwrap_or("").trim();
        if is_pdf(target) {
            targets.push(target.to_string());
        }
        rest = &after[end + 2..];
    }
    let mut rest = markdown;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let raw = after[..end].trim();
        let raw = raw.split_whitespace().next().unwrap_or("");
        let raw = raw.trim_matches(|ch| ch == '<' || ch == '>');
        let target = percent_decode(raw);
        let target = target.split('#').next().unwrap_or("").to_string();
        if is_pdf(&target) && !target.contains("://") {
            targets.push(target);
        }
        rest = &after[end + 1..];
    }
    targets
}

fn resolve_target(target: &str, note_dir: &str, vault_pdfs: &[String]) -> Option<String> {
    let cleaned = target
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    let candidates = [
        cleaned.to_string(),
        if note_dir.is_empty() {
            cleaned.to_string()
        } else {
            format!("{note_dir}/{cleaned}")
        },
    ];
    for candidate in &candidates {
        if let Some(found) = vault_pdfs
            .iter()
            .find(|path| path.eq_ignore_ascii_case(candidate))
        {
            return Some(found.clone());
        }
    }
    let name = cleaned
        .rsplit('/')
        .next()
        .unwrap_or(cleaned)
        .to_ascii_lowercase();
    let mut matches: Vec<&String> = vault_pdfs
        .iter()
        .filter(|path| {
            path.rsplit('/')
                .next()
                .is_some_and(|file| file.eq_ignore_ascii_case(&name))
        })
        .collect();
    if let Some(same_folder) = matches
        .iter()
        .find(|path| path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("") == note_dir)
    {
        return Some((*same_folder).clone());
    }
    matches.sort();
    matches.first().map(|path| (*path).clone())
}

fn percent_decode(value: &str) -> String {
    percent_encoding::percent_decode_str(value)
        .decode_utf8_lossy()
        .to_string()
}

/// Width and height (in PDF points, rotation applied) of every page.
pub fn pdf_page_sizes(bytes: &[u8]) -> Result<Vec<(f64, f64)>> {
    let document = lopdf::Document::load_mem(bytes).map_err(|error| anyhow!("{error}"))?;
    if document.is_encrypted() {
        bail!("PDF is encrypted");
    }
    let mut sizes = Vec::new();
    for (_, page_id) in document.get_pages() {
        let media_box = inherited(&document, page_id, b"MediaBox")
            .and_then(|object| numbers(&document, object))
            .filter(|values| values.len() == 4)
            .unwrap_or_else(|| vec![0.0, 0.0, 612.0, 792.0]);
        let mut width = (media_box[2] - media_box[0]).abs();
        let mut height = (media_box[3] - media_box[1]).abs();
        if width < 1.0 || height < 1.0 {
            width = 612.0;
            height = 792.0;
        }
        let rotate = inherited(&document, page_id, b"Rotate")
            .and_then(|object| resolve(&document, object).as_i64().ok())
            .unwrap_or(0)
            .rem_euclid(360);
        if rotate == 90 || rotate == 270 {
            std::mem::swap(&mut width, &mut height);
        }
        sizes.push((width, height));
    }
    if sizes.is_empty() {
        bail!("PDF has no pages");
    }
    Ok(sizes)
}

fn inherited<'a>(
    document: &'a lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Option<&'a lopdf::Object> {
    let mut current = page_id;
    for _ in 0..64 {
        let dictionary = document.get_dictionary(current).ok()?;
        if let Ok(value) = dictionary.get(key) {
            return Some(value);
        }
        current = dictionary.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn resolve<'a>(document: &'a lopdf::Document, object: &'a lopdf::Object) -> &'a lopdf::Object {
    match object.as_reference() {
        Ok(id) => document.get_object(id).unwrap_or(object),
        Err(_) => object,
    }
}

fn numbers(document: &lopdf::Document, object: &lopdf::Object) -> Option<Vec<f64>> {
    resolve(document, object)
        .as_array()
        .ok()?
        .iter()
        .map(|value| {
            let value = resolve(document, value);
            value
                .as_f32()
                .map(|v| v as f64)
                .or_else(|_| value.as_i64().map(|v| v as f64))
                .ok()
        })
        .collect()
}

/// The `.sbn2` document Saber itself writes after importing a PDF: one page per PDF page,
/// scaled to width 1000, the PDF as asset 0 used by every page background, plus the empty
/// trailing page Saber always keeps.
pub fn build_pdf_note(pages: &[(f64, f64)]) -> Document {
    let mut z: Vec<Bson> = pages
        .iter()
        .enumerate()
        .map(|(index, (width, height))| {
            let page_height = (PAGE_WIDTH * height / width).round();
            Bson::Document(doc! {
                "w": PAGE_WIDTH,
                "h": page_height,
                "b": {
                    "id": index as i32,
                    "e": ".pdf",
                    "i": index as i32,
                    "v": true,
                    "f": 1_i32,
                    "x": 0.0,
                    "y": 0.0,
                    "w": 0.0,
                    "h": 0.0,
                    "nw": *width,
                    "nh": *height,
                    "a": 0_i32,
                    "pdfi": index as i32,
                },
            })
        })
        .collect();
    z.push(Bson::Document(doc! { "w": PAGE_WIDTH, "h": 1400.0 }));
    doc! {
        "v": super::sbn::SUPPORTED_VERSION as i32,
        "ni": pages.len() as i32,
        "b": Bson::Null,
        "p": "",
        "l": 40_i32,
        "lt": 3_i32,
        "z": z,
        "c": 0_i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_body_and_frontmatter_tags() {
        assert!(has_tag("Read this #tablet later", "tablet"));
        assert!(has_tag("#Tablet/uni at the start", "tablet"));
        assert!(has_tag("tags: [#tablet]\n", "tablet"));
        assert!(!has_tag("a #tablets b", "tablet"));
        assert!(!has_tag("email#tablet", "tablet"));
        assert!(has_tag("---\ntags: [uni, tablet]\n---\nbody", "tablet"));
        assert!(has_tag(
            "---\ntags:\n  - Tablet\n  - other\n---\nbody",
            "tablet"
        ));
        assert!(has_tag("---\ntag: \"#tablet\"\n---\n", "tablet"));
        assert!(!has_tag("---\ntags: [tablets]\n---\nno", "tablet"));
        assert!(!has_tag("---\ntitle: tablet\n---\nno", "tablet"));
    }

    #[test]
    fn resolves_links_like_obsidian() {
        let pdfs = vec![
            "Uni/Slides/Lecture 1.pdf".to_string(),
            "Papers/Attention.pdf".to_string(),
            "Uni/Attention.pdf".to_string(),
        ];
        let markdown = "See ![[Lecture 1.pdf#page=3]] and [[Papers/Attention.pdf|the paper]] and [x](Slides/Lecture%201.pdf) and [w](https://example.com/x.pdf)";
        assert_eq!(
            resolve_pdf_links(markdown, "Uni/Notes.md", &pdfs),
            vec!["Uni/Slides/Lecture 1.pdf", "Papers/Attention.pdf"]
        );
        // A bare name prefers the note's own folder.
        assert_eq!(
            resolve_pdf_links("[[Attention.pdf]]", "Uni/Notes.md", &pdfs),
            vec!["Uni/Attention.pdf"]
        );
        assert_eq!(
            resolve_pdf_links("[[Attention.pdf]]", "Elsewhere/Notes.md", &pdfs),
            vec!["Papers/Attention.pdf"]
        );
        assert!(resolve_pdf_links("[[Missing.pdf]]", "Notes.md", &pdfs).is_empty());
    }

    #[test]
    fn maps_pdf_paths_to_saber_notes() {
        assert_eq!(
            saber_note_path("Uni/Slides/Lecture 1.pdf"),
            "/Uni/Slides/Lecture 1.sbn2"
        );
        assert_eq!(saber_note_path("x.PDF"), "/x.sbn2");
    }

    pub fn two_page_pdf() -> Vec<u8> {
        use pdf_writer::{Pdf, Rect, Ref};
        let mut pdf = Pdf::new();
        let catalog = Ref::new(1);
        let tree = Ref::new(2);
        let page_a = Ref::new(3);
        let page_b = Ref::new(4);
        pdf.catalog(catalog).pages(tree);
        pdf.pages(tree).kids([page_a, page_b]).count(2);
        pdf.page(page_a)
            .parent(tree)
            .media_box(Rect::new(0.0, 0.0, 595.0, 842.0));
        pdf.page(page_b)
            .parent(tree)
            .media_box(Rect::new(0.0, 0.0, 842.0, 595.0))
            .rotate(90);
        pdf.finish()
    }

    #[test]
    fn reads_page_sizes_and_builds_a_saber_note() {
        let sizes = pdf_page_sizes(&two_page_pdf()).unwrap();
        assert_eq!(sizes, vec![(595.0, 842.0), (595.0, 842.0)]);
        let note = build_pdf_note(&sizes);
        let mut bytes = Vec::new();
        note.to_writer(&mut bytes).unwrap();
        let parsed = super::super::sbn::Note::parse(&bytes).unwrap();
        assert_eq!(parsed.pages.len(), 3);
        assert_eq!(parsed.pages[0].width, 1000.0);
        assert_eq!(parsed.pages[0].height, 1415.0);
        let background = parsed.pages[0].background_image.as_ref().unwrap();
        assert_eq!(background.extension, ".pdf");
        assert_eq!(background.asset_index, Some(0));
        assert_eq!(background.pdf_page, Some(0));
        assert_eq!(
            parsed.pages[1].background_image.as_ref().unwrap().pdf_page,
            Some(1)
        );
        assert!(parsed.pages[2].is_empty());
        assert_eq!(parsed.asset_indices(), vec![0]);
        assert!(pdf_page_sizes(b"not a pdf").is_err());
    }
}
