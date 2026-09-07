//! Server-side support for the Saber handwriting app (https://github.com/saber-notes/saber).
//!
//! Saber syncs to Nextcloud over WebDAV with end-to-end encryption. This server emulates the
//! small part of Nextcloud that Saber talks to (see `crate::nextcloud`), and — when the user has
//! trusted it with their Saber encryption password — decrypts the uploaded notes and renders
//! them to PDFs inside the vault so Obsidian can show them.

pub mod crypto;
pub mod freehand;
pub mod pdf;
pub mod sbn;
pub mod sync;
