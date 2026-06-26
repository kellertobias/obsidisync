use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug)]
pub struct GitOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub async fn git(repo: Option<&Path>, args: &[&str], allowed_codes: &[i32]) -> Result<GitOutput> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = repo {
        command.current_dir(cwd);
    }
    let output = command.output().await?;
    let code = output.status.code().unwrap_or(1);
    let result = GitOutput {
        code,
        stdout: output.stdout,
        stderr: output.stderr,
    };
    if allowed_codes.contains(&code) {
        Ok(result)
    } else {
        Err(anyhow!(
            "git {} failed with {code}: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr)
        ))
    }
}

pub async fn git_strings(repo: &Path, args: &[String], allowed_codes: &[i32]) -> Result<GitOutput> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(Some(repo), &refs, allowed_codes).await
}
