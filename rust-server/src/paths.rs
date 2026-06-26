use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

pub fn validate_slug(input: &str, kind: &str) -> Result<String> {
    if input.is_empty() || input.len() > 96 {
        bail!("invalid {kind}");
    }
    if !input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        bail!("invalid {kind}");
    }
    if input == "." || input == ".." || input.starts_with('.') {
        bail!("invalid {kind}");
    }
    Ok(input.to_string())
}

pub fn validate_git_branch(input: &str) -> Result<String> {
    let branch = input.trim();
    if branch.is_empty() {
        return Ok("main".to_string());
    }
    if branch.len() > 255
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("//")
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains('\\')
        || branch.chars().any(|ch| {
            ch.is_control()
                || ch.is_whitespace()
                || !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/'))
        })
    {
        bail!("invalid branch");
    }
    if branch
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == ".." || part.ends_with(".lock"))
    {
        bail!("invalid branch");
    }
    Ok(branch.to_string())
}

pub fn validate_git_identity(input: &str, kind: &str) -> Result<String> {
    let value = input.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(|ch| ch.is_control()) {
        bail!("invalid {kind}");
    }
    Ok(value.to_string())
}

pub fn validate_commit_id(input: &str) -> Result<String> {
    let value = input.trim();
    if !matches!(value.len(), 40 | 64)
        || !value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("invalid commit id");
    }
    Ok(value.to_string())
}

pub fn sanitize_commit_component(input: &str) -> String {
    let mut value: String = input
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if value.len() > 80 {
        value.truncate(80);
    }
    if value.is_empty() {
        "device".to_string()
    } else {
        value
    }
}

pub fn validate_vault_path(input: &str) -> Result<String> {
    if input.is_empty() || input.contains('\0') || input.starts_with('/') || input.contains('\\') {
        bail!("unsafe vault path: {input}");
    }
    let mut parts = Vec::new();
    for part in input.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            bail!("unsafe vault path: {input}");
        }
        if part == ".git" {
            bail!("unsafe vault path: {input}");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub fn repo_path(repo_root: &Path, vault_path: &str) -> Result<PathBuf> {
    let safe = validate_vault_path(vault_path)?;
    let root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let absolute = root.join(safe);
    if absolute.starts_with(&root) || !root.exists() {
        Ok(absolute)
    } else {
        Err(anyhow!("unsafe vault path: {vault_path}"))
    }
}

pub fn is_text_or_code_path(path: &str) -> bool {
    let lower = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let extension = lower.rsplit('.').next().unwrap_or("");
    matches!(
        extension,
        "md" | "markdown"
            | "txt"
            | "csv"
            | "tsv"
            | "json"
            | "jsonc"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "html"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
            | "py"
            | "rb"
            | "rs"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cs"
            | "php"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "sql"
            | "graphql"
            | "gql"
            | "ini"
            | "conf"
            | "env"
            | "gitignore"
            | "dockerfile"
            | "r"
            | "lua"
            | "ex"
            | "exs"
            | "erl"
            | "hrl"
            | "clj"
            | "cljs"
            | "scala"
            | "vim"
            | "tex"
            | "bib"
    ) || matches!(
        lower.as_str(),
        "dockerfile" | "makefile" | "justfile" | "gemfile" | "rakefile" | ".gitignore"
    )
}
