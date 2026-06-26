use anyhow::{bail, Result};
use std::path::Path;
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct RemotePolicy {
    pub allow_local_remotes: bool,
    pub allowed_hosts: Vec<String>,
}

impl RemotePolicy {
    pub fn validate(&self, remote_url: &str) -> Result<()> {
        let remote = remote_url.trim();
        if remote.is_empty()
            || remote.len() > 2048
            || remote.starts_with('-')
            || remote
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            bail!("invalid git remote URL");
        }

        if is_local_remote(remote) {
            if self.allow_local_remotes {
                return Ok(());
            }
            bail!("local git remotes are disabled");
        }

        if let Ok(url) = Url::parse(remote) {
            return self.validate_url(&url);
        }

        if let Some((username, host)) = scp_like_parts(remote) {
            if let Some(username) = username {
                validate_ssh_username(username)?;
            }
            return self.validate_host(host);
        }

        bail!("git remote URL must use https, ssh, or scp-like SSH syntax")
    }

    fn validate_url(&self, url: &Url) -> Result<()> {
        match url.scheme() {
            "https" => {
                if !url.username().is_empty() || url.password().is_some() {
                    bail!("git remote URL must not include embedded credentials");
                }
            }
            "ssh" => {
                if url.password().is_some() {
                    bail!("git remote URL must not include embedded credentials");
                }
                validate_ssh_username(url.username())?;
            }
            "file" => {
                if self.allow_local_remotes {
                    return Ok(());
                }
                bail!("local git remotes are disabled");
            }
            _ => bail!("git remote URL must use https or ssh"),
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("git remote URL must include a host"))?;
        self.validate_host(host)
    }

    fn validate_host(&self, host: &str) -> Result<()> {
        let normalized = normalize_host(host);
        if normalized.is_empty() {
            bail!("git remote URL must include a host");
        }
        if normalized.starts_with('-')
            || normalized.contains("..")
            || normalized
                .chars()
                .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | ':')))
        {
            bail!("git remote host is invalid");
        }
        if self.allowed_hosts.is_empty() {
            bail!("no git remote hosts are configured");
        }
        if !self.allowed_hosts.is_empty()
            && !self
                .allowed_hosts
                .iter()
                .any(|allowed| host_matches(&normalized, allowed))
        {
            bail!("git remote host is not allowed");
        }
        Ok(())
    }
}

fn is_local_remote(remote: &str) -> bool {
    remote.starts_with("file://")
        || Path::new(remote).is_absolute()
        || remote.starts_with("./")
        || remote.starts_with("../")
}

fn scp_like_parts(remote: &str) -> Option<(Option<&str>, &str)> {
    let (before_colon, after_colon) = remote.split_once(':')?;
    if before_colon.contains('/') || after_colon.is_empty() {
        return None;
    }
    Some(match before_colon.rsplit_once('@') {
        Some((username, host)) => (Some(username), host),
        None => (None, before_colon),
    })
}

fn validate_ssh_username(username: &str) -> Result<()> {
    if username.is_empty() {
        return Ok(());
    }
    if username.starts_with('-')
        || username.len() > 128
        || username
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        bail!("git remote SSH username is invalid");
    }
    Ok(())
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn host_matches(host: &str, allowed: &str) -> bool {
    let allowed = normalize_host(allowed);
    if let Some(suffix) = allowed.strip_prefix("*.") {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else {
        host == allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_local_and_credentialed_remotes_by_default() {
        let policy = RemotePolicy::default();
        assert!(policy.validate("/tmp/repo.git").is_err());
        assert!(policy.validate("file:///tmp/repo.git").is_err());
        assert!(policy
            .validate("https://github.com/example/repo.git")
            .is_err());
        assert!(policy
            .validate("https://token@github.com/example/repo.git")
            .is_err());
        assert!(policy.validate("https://localhost/repo.git").is_err());
        assert!(policy.validate("https://192.168.1.2/repo.git").is_err());
        assert!(policy.validate("git@-github.com:example/repo.git").is_err());
        assert!(policy
            .validate("-oProxyCommand=example:example/repo.git")
            .is_err());
    }

    #[test]
    fn accepts_https_and_ssh_with_allowed_hosts() {
        let policy = RemotePolicy {
            allow_local_remotes: false,
            allowed_hosts: vec![
                "github.com".to_string(),
                "*.example.com".to_string(),
                "192.168.1.2".to_string(),
            ],
        };
        assert!(policy
            .validate("https://github.com/example/repo.git")
            .is_ok());
        assert!(policy.validate("git@github.com:example/repo.git").is_ok());
        assert!(policy
            .validate("ssh://git@git.example.com/example/repo.git")
            .is_ok());
        assert!(policy.validate("https://192.168.1.2/repo.git").is_ok());
        assert!(policy
            .validate("https://gitlab.com/example/repo.git")
            .is_err());
    }

    #[test]
    fn local_remotes_require_explicit_policy() {
        let policy = RemotePolicy {
            allow_local_remotes: true,
            allowed_hosts: Vec::new(),
        };
        assert!(policy.validate("/tmp/repo.git").is_ok());
        assert!(policy.validate("file:///tmp/repo.git").is_ok());
    }
}
