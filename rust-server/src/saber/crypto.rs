//! Saber's client-side encryption, reimplemented so the server can read what the app uploads.
//!
//! Saber (https://github.com/saber-notes/saber) encrypts every note before it reaches Nextcloud:
//!
//! - The AES-256 key is `SHA-256(encryption_password + "8MnPs64@R&mF8XjWeLrD")`. The salt is a
//!   constant in the app; the password is the "encryption password" the user chooses at login
//!   and never sends to the server.
//! - The IV is generated once per account, stored base64 in `Saber/config.sbc` (a plain JSON
//!   file) next to a copy of a random key that is itself encrypted with the password. Saber
//!   only uses that stored key to check the password; the file contents are encrypted with the
//!   password-derived key above.
//! - Files are AES/SIC (CTR with a big-endian 128-bit counter) with PKCS#7 padding applied to
//!   the plaintext first, which is what Dart's `encrypt` package does for `AES(key)` with its
//!   default `AESMode.sic` and `PKCS7` padding.
//! - File names are the note's vault-relative path (for example `/Uni/Lecture 1.sbn2`) run
//!   through the same cipher and hex-encoded, plus the `.sbe` extension.

use aes::Aes256;
use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha256};

const REPRODUCIBLE_SALT: &str = "8MnPs64@R&mF8XjWeLrD";
/// Extension of every encrypted note or asset on the server.
pub const ENCRYPTED_EXTENSION: &str = ".sbe";
/// The plain JSON file that carries the IV and the password check.
pub const CONFIG_FILE_NAME: &str = "config.sbc";

type Cipher = ctr::Ctr128BE<Aes256>;

#[derive(Clone)]
pub struct SaberCipher {
    key: [u8; 32],
    iv: [u8; 16],
}

impl std::fmt::Debug for SaberCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SaberCipher(..)")
    }
}

impl SaberCipher {
    /// Builds the cipher from the user's Saber encryption password and the IV found in
    /// `config.sbc`.
    pub fn new(encryption_password: &str, iv_base64: &str) -> Result<Self> {
        let iv_bytes = STANDARD
            .decode(iv_base64.trim())
            .map_err(|_| anyhow!("saber config: iv is not valid base64"))?;
        let iv: [u8; 16] = iv_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("saber config: iv must be 16 bytes"))?;
        Ok(Self {
            key: derive_key(encryption_password),
            iv,
        })
    }

    /// Reads the IV from the JSON body of `config.sbc` and verifies the password against the
    /// stored key check. Fails when the password is wrong.
    pub fn from_config(encryption_password: &str, config_json: &[u8]) -> Result<Self> {
        let config: serde_json::Value = serde_json::from_slice(config_json)
            .map_err(|error| anyhow!("saber config: not valid JSON: {error}"))?;
        let iv = config
            .get("iv")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("saber config: missing iv"))?;
        let cipher = Self::new(encryption_password, iv)?;
        if let Some(key_check) = config.get("key").and_then(|value| value.as_str()) {
            let encrypted = STANDARD
                .decode(key_check.trim())
                .map_err(|_| anyhow!("saber config: key is not valid base64"))?;
            let decrypted = cipher
                .decrypt(&encrypted)
                .map_err(|_| anyhow!("saber encryption password does not match this account"))?;
            // Saber stores the base64 of a 32 byte key: 44 ASCII characters.
            if decrypted.len() != 44 || !decrypted.iter().all(u8::is_ascii) {
                bail!("saber encryption password does not match this account");
            }
        }
        Ok(cipher)
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut buffer = plaintext.to_vec();
        let pad = 16 - (buffer.len() % 16);
        buffer.extend(std::iter::repeat_n(pad as u8, pad));
        let mut cipher = Cipher::new(&self.key.into(), &self.iv.into());
        cipher.apply_keystream(&mut buffer);
        buffer
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
            bail!("saber ciphertext length is not a multiple of the block size");
        }
        let mut buffer = ciphertext.to_vec();
        let mut cipher = Cipher::new(&self.key.into(), &self.iv.into());
        cipher.apply_keystream(&mut buffer);
        let pad = *buffer.last().unwrap_or(&0) as usize;
        if pad == 0 || pad > 16 || pad > buffer.len() {
            bail!("saber ciphertext has invalid padding");
        }
        if !buffer[buffer.len() - pad..]
            .iter()
            .all(|byte| *byte as usize == pad)
        {
            bail!("saber ciphertext has invalid padding");
        }
        buffer.truncate(buffer.len() - pad);
        Ok(buffer)
    }

    /// The server-side file name (without directory) Saber uses for a note path such as
    /// `/Uni/Lecture 1.sbn2`.
    pub fn encrypt_file_name(&self, note_path: &str) -> String {
        let mut name = hex_encode(&self.encrypt(note_path.as_bytes()));
        name.push_str(ENCRYPTED_EXTENSION);
        name
    }

    /// Recovers the note path from an encrypted file name. Returns `None` for files that are
    /// not Saber notes (for example `config.sbc` or a stray README).
    pub fn decrypt_file_name(&self, file_name: &str) -> Option<String> {
        let hex = file_name.strip_suffix(ENCRYPTED_EXTENSION)?;
        let bytes = hex_decode(hex)?;
        let decrypted = self.decrypt(&bytes).ok()?;
        let mut path = String::from_utf8(decrypted).ok()?;
        // Saber mitigates an old import bug where paths started with `null/` instead of `/`.
        if let Some(rest) = path.strip_prefix("null/") {
            path = format!("/{rest}");
        }
        Some(path)
    }
}

fn derive_key(password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(REPRODUCIBLE_SALT.as_bytes());
    hasher.finalize().into()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> SaberCipher {
        SaberCipher::new("hunter2", &STANDARD.encode([7_u8; 16])).unwrap()
    }

    #[test]
    fn round_trips_bytes_and_names() {
        let cipher = cipher();
        let data = b"hello saber".to_vec();
        let encrypted = cipher.encrypt(&data);
        assert_eq!(encrypted.len(), 16);
        assert_eq!(cipher.decrypt(&encrypted).unwrap(), data);

        let name = cipher.encrypt_file_name("/Uni/Lecture 1.sbn2");
        assert!(name.ends_with(".sbe"));
        assert_eq!(
            cipher.decrypt_file_name(&name).as_deref(),
            Some("/Uni/Lecture 1.sbn2")
        );
        assert_eq!(cipher.decrypt_file_name("config.sbc"), None);
        assert_eq!(cipher.decrypt_file_name("zz.sbe"), None);
    }

    #[test]
    fn matches_openssl_aes_256_ctr_with_pkcs7() {
        // openssl enc -aes-256-ctr -K sha256("hunter2" + salt) -iv 0707..07 over "abc" plus
        // thirteen 0x0d padding bytes.
        let cipher = cipher();
        let encrypted = cipher.encrypt(b"abc");
        assert_eq!(hex_encode(&encrypted), "77e13cb179bef3e119a6202c6875ae1e");
        let mut broken = encrypted.clone();
        broken[15] ^= 0xff;
        assert!(cipher.decrypt(&broken).is_err());
        assert!(cipher.decrypt(&encrypted[..8]).is_err());
    }

    #[test]
    fn verifies_password_against_config() {
        let cipher = cipher();
        let key_check = STANDARD.encode(cipher.encrypt(STANDARD.encode([1_u8; 32]).as_bytes()));
        let config = serde_json::json!({ "iv": STANDARD.encode([7_u8; 16]), "key": key_check });
        let bytes = serde_json::to_vec(&config).unwrap();
        assert!(SaberCipher::from_config("hunter2", &bytes).is_ok());
        assert!(SaberCipher::from_config("wrong", &bytes).is_err());
        let no_check =
            serde_json::to_vec(&serde_json::json!({ "iv": STANDARD.encode([7_u8; 16]) })).unwrap();
        assert!(SaberCipher::from_config("anything", &no_check).is_ok());
    }
}
