use anyhow::{bail, Result};
use obsidian_git_sync_server::auth::AuthVerifier;
use obsidian_git_sync_server::http::{router, AppState};
use obsidian_git_sync_server::remote::RemotePolicy;
use obsidian_git_sync_server::vault::{VaultService, VaultServiceOptions};
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = RuntimeConfig::from_env()?;
    let vaults = VaultService::new_with_options(VaultServiceOptions {
        data_dir: config.data_dir,
        remote_policy: config.remote_policy,
    });
    let app = router(
        AppState {
            vaults,
            auth: config.auth,
        },
        config.max_body_bytes,
        config.allowed_origins,
    );
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!("obsidian git sync server listening on {}", config.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

struct RuntimeConfig {
    listen: SocketAddr,
    data_dir: PathBuf,
    auth: AuthVerifier,
    max_body_bytes: usize,
    remote_policy: RemotePolicy,
    allowed_origins: Vec<String>,
}

impl RuntimeConfig {
    fn from_env() -> Result<Self> {
        let port = std::env::var("PORT")
            .unwrap_or_else(|_| "8787".to_string())
            .parse::<u16>()?;
        let listen = std::env::var("OBSIDIAN_GIT_SYNC_LISTEN")
            .unwrap_or_else(|_| format!("127.0.0.1:{port}"))
            .parse::<SocketAddr>()?;
        let data_dir = PathBuf::from(
            std::env::var("OBSIDIAN_GIT_SYNC_DATA_DIR").unwrap_or_else(|_| "data".to_string()),
        );

        let auth = if let Ok(token) = std::env::var("OBSIDIAN_GIT_SYNC_DEV_TOKEN") {
            let user =
                std::env::var("OBSIDIAN_GIT_SYNC_DEV_USER").unwrap_or_else(|_| "dev".to_string());
            AuthVerifier::StaticTokenForDev { token, user }
        } else if let Some(user) = password_user_env() {
            AuthVerifier::password(user, data_dir.clone())?
        } else {
            let issuer = required_env("OIDC_ISSUER")?;
            let audience = required_env("OIDC_AUDIENCE")?;
            let jwks_url = std::env::var("OIDC_JWKS_URL").ok();
            let user_claim = std::env::var("OIDC_USER_CLAIM")
                .unwrap_or_else(|_| "preferred_username".to_string());
            AuthVerifier::oidc(issuer, audience, jwks_url, user_claim)?
        };
        let max_body_bytes = std::env::var("OBSIDIAN_GIT_SYNC_MAX_BODY_BYTES")
            .unwrap_or_else(|_| (50 * 1024 * 1024).to_string())
            .parse::<usize>()?;
        let remote_policy = RemotePolicy {
            allow_local_remotes: parse_bool_env("OBSIDIAN_GIT_SYNC_ALLOW_LOCAL_REMOTES"),
            allowed_hosts: parse_csv_env("OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS"),
        };
        let allowed_origins = parse_csv_env("OBSIDIAN_GIT_SYNC_ALLOWED_ORIGINS");

        Ok(Self {
            listen,
            data_dir,
            auth,
            max_body_bytes,
            remote_policy,
            allowed_origins,
        })
    }
}

fn password_user_env() -> Option<String> {
    std::env::var("OBSIDIAN_GIT_SYNC_PASSWORD_USER")
        .or_else(|_| std::env::var("OBSIDIAN_GIT_SYNC_USER"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_env(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("{name} is required unless OBSIDIAN_GIT_SYNC_DEV_TOKEN or OBSIDIAN_GIT_SYNC_PASSWORD_USER is set"),
    }
}

fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn parse_csv_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}
