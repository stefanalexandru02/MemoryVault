//! Project key resolution (spec 8.4).

use std::path::Path;
use std::process::Command;

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `https://user:tok@GitHub.com/Alex/Homelab.git`, `git@github.com:alex/homelab.git` and
/// `ssh://git@github.com:22/alex/homelab` all become `github.com/alex/homelab`.
pub fn normalize_remote(url: &str) -> String {
    let url = url.trim();
    let (has_scheme, rest) = match url.split_once("://") {
        Some((_, rest)) => (true, rest),
        None => (false, url),
    };
    let slash = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(slash);
    // Drop userinfo (`user`, `user:password`).
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let mut key = if has_scheme {
        // URL form: a numeric port is not part of the identity.
        match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                format!("{host}{path}")
            }
            _ => format!("{authority}{path}"),
        }
    } else {
        // scp-like `host:owner/repo`: the colon separates host from path.
        match authority.split_once(':') {
            Some((host, first)) => format!("{host}/{first}{path}"),
            None => format!("{authority}{path}"),
        }
    };
    key = key.trim_end_matches('/').to_string();
    if let Some(stripped) = key.strip_suffix(".git") {
        key = stripped.to_string();
    }
    key.trim_matches('/').to_lowercase()
}

fn basename(path: &str) -> Option<String> {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
}

/// The repo name for a repository without `origin`: worktrees resolve through the common git dir
/// to the main repository, so they share the key of the main checkout.
fn local_repo_name(cwd: &str) -> Option<String> {
    let common = git(cwd, &["rev-parse", "--git-common-dir"])?;
    let common = if Path::new(&common).is_absolute() {
        common
    } else {
        Path::new(cwd).join(&common).to_string_lossy().into_owned()
    };
    let common = std::fs::canonicalize(&common).ok()?;
    let name = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().and_then(|p| p.file_name())
    } else {
        common.file_name() // bare repository `name.git`
    }?;
    let name = name.to_string_lossy();
    Some(name.strip_suffix(".git").unwrap_or(&name).to_string())
}

pub fn resolve(cwd: &str) -> String {
    if let Some(url) = git(cwd, &["remote", "get-url", "origin"]) {
        let key = normalize_remote(&url);
        if !key.is_empty() {
            return key;
        }
    }
    if let Some(name) =
        local_repo_name(cwd).or_else(|| git(cwd, &["rev-parse", "--show-toplevel"]).and_then(|t| basename(&t)))
    {
        return format!("local/{name}");
    }
    format!("local/{}", basename(cwd).unwrap_or_else(|| "unknown".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remotes() {
        for (input, want) in [
            ("https://github.com/alex/homelab.git", "github.com/alex/homelab"),
            ("git@github.com:alex/homelab.git", "github.com/alex/homelab"),
            ("ssh://git@GitHub.com:22/Alex/Homelab", "github.com/alex/homelab"),
            ("https://user:secret@gitlab.example.com/grp/sub/proj.git/", "gitlab.example.com/grp/sub/proj"),
            ("git://host.example/x/y.git", "host.example/x/y"),
            ("github.com:alex/homelab", "github.com/alex/homelab"),
        ] {
            assert_eq!(normalize_remote(input), want, "{input}");
        }
    }
}
