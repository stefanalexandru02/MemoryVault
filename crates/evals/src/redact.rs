//! `memvault evals redact`: mask secrets, emails, tokens and home paths in recorded sessions.
//!
//! Every JSON string is rewritten (keys are never changed). A string value under a key whose name
//! suggests a credential is replaced whole. Redaction is idempotent.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::{Captures, Regex};
use serde_json::Value;

struct Rule {
    re: Regex,
    to: &'static str,
}

fn rule(re: &str, to: &'static str) -> Rule {
    Rule { re: Regex::new(re).expect("valid redaction pattern"), to }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)", "<private-key>"),
        rule(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "<jwt>"),
        rule(r"\bmem_[A-Za-z0-9]{8}_[A-Za-z0-9]{32}\b", "<token>"),
        rule(r"\b(?:sk|pk|rk)-[A-Za-z0-9_-]{16,}", "<token>"),
        rule(r"\bgh[pousr]_[A-Za-z0-9]{20,}", "<token>"),
        rule(r"\bgithub_pat_[A-Za-z0-9_]{20,}", "<token>"),
        rule(r"\bxox[abprs]-[A-Za-z0-9-]{10,}", "<token>"),
        rule(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "<token>"),
        rule(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{12,}", "Bearer <token>"),
        rule(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}", "<email>"),
        rule(r"(?:/home|/Users)/[^/\s\x22'\\]+", "~"),
        rule(r"(?i)[A-Z]:\\Users\\[^\\\s\x22']+", "~"),
        rule(r"/root\b", "~"),
    ]
});

/// `password = hunter2-x9`, `api_key: "abcd1234"`. Needs 8+ characters with a digit or symbol so
/// prose such as "token: parser" survives.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|secret|api[_-]?key|access[_-]?key|auth[_-]?token|token)(\s*[=:]\s*)(["']?)([^\s"',;]{8,})"#,
    )
    .expect("valid assignment pattern")
});

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(password|passwd|secret|api[_-]?key|authorization|credential|private[_-]?key)").unwrap()
});

pub fn redact_str(s: &str) -> String {
    let mut out = s.to_string();
    for r in RULES.iter() {
        out = r.re.replace_all(&out, r.to).into_owned();
    }
    ASSIGNMENT
        .replace_all(&out, |c: &Captures| {
            let value = &c[4];
            if value.chars().any(|ch| !ch.is_alphabetic()) && !value.starts_with('<') {
                format!("{}{}{}<secret>", &c[1], &c[2], &c[3])
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

pub fn redact_value(v: &mut Value) {
    match v {
        Value::String(s) => *s = redact_str(s),
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                match val {
                    Value::String(_) if SECRET_KEY.is_match(k) => *val = Value::String("<secret>".into()),
                    _ => redact_value(val),
                }
            }
        }
        _ => {}
    }
}

/// Redact JSONL `text` line by line (the `external_session_id` is kept).
pub fn redact_text(text: &str) -> Result<String> {
    let mut out = String::new();
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let mut v: Value = serde_json::from_str(raw).with_context(|| format!("line {}: invalid JSON", i + 1))?;
        let keep = v.get("external_session_id").cloned();
        redact_value(&mut v);
        if let (Some(keep), Some(obj)) = (keep, v.as_object_mut()) {
            obj.insert("external_session_id".into(), keep);
        }
        out.push_str(&v.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// Redact a file in place, or into `out` when given. Returns the number of changed lines.
pub fn redact_file(path: &Path, out: Option<&Path>) -> Result<usize> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let redacted = redact_text(&text)?;
    let changed = text
        .lines()
        .zip(redacted.lines())
        .filter(|(a, b)| serde_json::from_str::<Value>(a).ok() != serde_json::from_str::<Value>(b).ok())
        .count();
    std::fs::write(out.unwrap_or(path), redacted)
        .with_context(|| format!("writing {}", out.unwrap_or(path).display()))?;
    Ok(changed)
}
