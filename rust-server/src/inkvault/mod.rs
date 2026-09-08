//! InkNote v1 validation and deterministic PDF rendering, independent of HTTP/storage.
pub mod cbor;
mod render;
use crate::paths::validate_vault_path;
use anyhow::{anyhow, ensure, Result};
pub use render::render;
use serde_json::Value;
use std::collections::BTreeSet;

pub const FEATURE: &str = "inkVaultNotesV1";
pub const PREFIX: &str = ".inkvault/notes/";
pub fn is_source(path: &str) -> bool {
    path.starts_with(".inkvault/")
}
pub fn document_root(path: &str) -> Result<String> {
    validate_vault_path(path)?;
    let rest = path
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow!("invalid InkNote path"))?;
    let id = rest.split('/').next().unwrap_or_default();
    ensure!(uuid(id), "invalid InkNote document ID");
    Ok(format!("{PREFIX}{id}"))
}
pub fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit() && !c.is_ascii_uppercase()
            }
        })
}
pub fn integer(value: &Value, key: &str) -> Result<i64> {
    value[key]
        .as_i64()
        .ok_or_else(|| anyhow!("missing integer {key}"))
}
pub fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow!("missing string {key}"))
}
pub fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    value[key]
        .as_array()
        .ok_or_else(|| anyhow!("missing array {key}"))
}
pub fn manifest(bytes: &[u8], root: &str) -> Result<Value> {
    ensure!(bytes.len() <= 1024 * 1024, "manifest exceeds 1 MiB");
    let m: Value = serde_json::from_slice(bytes)?;
    ensure!(
        integer(&m, "schemaVersion")? == 1,
        "unsupported InkNote schema"
    );
    ensure!(
        text(&m, "documentId")? == root.rsplit('/').next().unwrap_or_default(),
        "document ID/path mismatch"
    );
    let path = text(&m, "pdfPath")?;
    validate_vault_path(path)?;
    ensure!(
        !path.split('/').any(|part| part.starts_with('.'))
            && path.to_ascii_lowercase().ends_with(".pdf"),
        "invalid visible PDF path"
    );
    ensure!(
        (1..=500).contains(&array(&m, "pages")?.len()),
        "InkNote must have 1–500 pages"
    );
    ensure!(
        integer(&m, "sourceRevision")? > 0,
        "invalid source revision"
    );
    ensure!(
        integer(&m, "created")? >= 0 && integer(&m, "modified")? >= 0,
        "invalid timestamps"
    );
    ensure!(text(&m, "title")?.len() <= 4096, "title too long");
    let mut ids = BTreeSet::new();
    for page in array(&m, "pages")? {
        let id = text(page, "id")?;
        ensure!(uuid(id) && ids.insert(id), "duplicate/invalid page ID");
        let (w, h) = (integer(page, "width")?, integer(page, "height")?);
        if m.get("basePdfHash").is_none() {
            ensure!(
                (w, h) == (210000, 297000) || (w, h) == (297000, 210000),
                "handwritten pages must be A4"
            );
        }
        ensure!(
            (1000..=2000000).contains(&w) && (1000..=2000000).contains(&h),
            "invalid page dimensions"
        );
        ensure!(
            text(page, "orientation")? == if w > h { "landscape" } else { "portrait" },
            "orientation mismatch"
        );
    }
    Ok(m)
}
