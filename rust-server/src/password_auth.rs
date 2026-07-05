use crate::app_session::{AppSession, AppSessionStore};
use crate::auth::normalize_user_claim;
use anyhow::{anyhow, bail, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::Mutex;

const MIN_PASSWORD_BYTES: usize = 12;

#[derive(Debug)]
pub struct PasswordAuth {
    user: String,
    setup_token_hash: Option<String>,
    store_path: PathBuf,
    sessions: AppSessionStore,
    lock: Mutex<()>,
}

pub type PasswordAuthSession = AppSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasswordStore {
    user: String,
    password_hash: Option<String>,
    #[serde(default)]
    sessions: Vec<PasswordSessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasswordSessionRecord {
    token_hash: String,
    created_at: u64,
}

impl PasswordAuth {
    pub fn new(
        user: String,
        data_dir: impl Into<PathBuf>,
        setup_token: Option<String>,
    ) -> Result<Self> {
        let data_dir = data_dir.into();
        let user = normalize_user_claim(&user)?;
        let setup_token_hash = setup_token
            .as_deref()
            .map(validate_setup_token)
            .transpose()?
            .map(hash_token);
        Ok(Self {
            user,
            setup_token_hash,
            store_path: data_dir.join("auth/password.json"),
            sessions: AppSessionStore::new(data_dir),
            lock: Mutex::new(()),
        })
    }

    pub async fn is_configured(&self) -> Result<bool> {
        Ok(self.read_store().await?.password_hash.is_some())
    }

    pub async fn setup_password(
        &self,
        username: &str,
        password: &str,
        setup_token: Option<&str>,
    ) -> Result<PasswordAuthSession> {
        let _guard = self.lock.lock().await;
        self.verify_setup_token(setup_token)?;
        self.ensure_setup_user(username)?;
        validate_password(password)?;

        let mut store = self.read_store().await?;
        if store.password_hash.is_some() {
            bail!("password is already set");
        }

        store.password_hash = Some(hash_password(password)?);
        self.write_store(&store).await?;
        self.sessions
            .issue(self.user.clone(), self.user.clone())
            .await
    }

    pub fn setup_token_is_required(&self) -> bool {
        self.setup_token_hash.is_some()
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<PasswordAuthSession> {
        let _guard = self.lock.lock().await;
        self.ensure_login_user(username)?;

        let store = self.read_store().await?;
        let password_hash = store
            .password_hash
            .as_deref()
            .ok_or_else(|| anyhow!("password is not set"))?;
        if !verify_password(password, password_hash)? {
            bail!("invalid username or password");
        }

        self.write_store(&store).await?;
        self.sessions
            .issue(self.user.clone(), self.user.clone())
            .await
    }

    pub async fn verify_token(&self, token: &str) -> Result<String> {
        if self.read_store().await?.password_hash.is_none() {
            bail!("password is not set");
        }
        Ok(self.sessions.verify_access_token(token).await?.user)
    }

    pub async fn refresh_session(&self, refresh_token: &str) -> Result<AppSession> {
        if self.read_store().await?.password_hash.is_none() {
            bail!("password is not set");
        }
        self.sessions
            .refresh(refresh_token, |user, subject| async move {
                let expected = normalize_user_claim(&user)?;
                if subject == expected {
                    Ok(())
                } else {
                    bail!("password session subject is invalid")
                }
            })
            .await
    }

    fn ensure_setup_user(&self, username: &str) -> Result<()> {
        let user = normalize_user_claim(username)?;
        if user == self.user {
            Ok(())
        } else {
            bail!("forbidden: invalid username")
        }
    }

    fn ensure_login_user(&self, username: &str) -> Result<()> {
        let user = normalize_user_claim(username)?;
        if user == self.user {
            Ok(())
        } else {
            bail!("invalid username or password")
        }
    }

    fn verify_setup_token(&self, setup_token: Option<&str>) -> Result<()> {
        let Some(expected_hash) = &self.setup_token_hash else {
            return Ok(());
        };
        let token = setup_token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("password setup token is required"))?;
        validate_setup_token(token)?;
        let token_hash = hash_token(token);
        if constant_time_eq(&token_hash, expected_hash) {
            Ok(())
        } else {
            bail!("invalid password setup token")
        }
    }

    async fn read_store(&self) -> Result<PasswordStore> {
        match fs::read_to_string(&self.store_path).await {
            Ok(contents) => {
                let store: PasswordStore = serde_json::from_str(&contents)?;
                if store.user != self.user {
                    bail!(
                        "password auth store belongs to {}, but configured user is {}",
                        store.user,
                        self.user
                    );
                }
                Ok(store)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PasswordStore {
                user: self.user.clone(),
                password_hash: None,
                sessions: Vec::new(),
            }),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_store(&self, store: &PasswordStore) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temp_path = temp_store_path(&self.store_path);
        fs::write(&temp_path, serde_json::to_vec_pretty(store)?).await?;
        fs::rename(temp_path, &self.store_path).await?;
        Ok(())
    }
}

fn validate_password(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_BYTES {
        bail!("password must be at least {MIN_PASSWORD_BYTES} bytes");
    }
    Ok(())
}

fn validate_setup_token(token: &str) -> Result<&str> {
    if token.trim() != token || token.len() < 16 || token.chars().any(char::is_whitespace) {
        bail!("invalid password setup token")
    }
    Ok(token)
}

fn hash_password(password: &str) -> Result<String> {
    let mut salt_bytes = [0_u8; 16];
    getrandom::fill(&mut salt_bytes)
        .map_err(|error| anyhow!("random generator failed: {error}"))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|error| anyhow!("password salt generation failed: {error}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("password hashing failed: {error}"))?
        .to_string())
}

fn verify_password(password: &str, password_hash: &str) -> Result<bool> {
    let parsed_hash = PasswordHash::new(password_hash)
        .map_err(|error| anyhow!("stored password hash is invalid: {error}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
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
