//! Prompt registry (spec 10.1): `prompts/*.yaml` loaded at start, one prompt per file with `name`,
//! `version`, `system`, `user_template`, `output_schema`, `model_hint`. `prompt_ref = name@version`;
//! changing text requires a new version and packets pin the version they were built with.
//!
//! Templates are minijinja with strict undefined (a template naming a field the packet inputs lack
//! fails loudly), no auto-escaping and trailing newlines kept. `tojson` emits plain JSON (no HTML
//! escaping); `tojson(2)` indents.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};

use crate::schemas::prompts::{PromptSpec, RenderedPrompt};

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("unknown prompt {0:?}")]
    Unknown(String),
    #[error("rendering {prompt_ref}: {source:#}")]
    Render {
        prompt_ref: String,
        #[source]
        source: minijinja::Error,
    },
}

/// Loaded prompts, by name then version number.
#[derive(Debug, Default)]
pub struct PromptRegistry {
    prompts: HashMap<String, BTreeMap<u32, PromptSpec>>,
    env: Environment<'static>,
}

fn tojson(value: minijinja::Value, indent: Option<usize>) -> Result<String, minijinja::Error> {
    let to_err = |e: serde_json::Error| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string());
    match indent {
        Some(n) if n > 0 => {
            let mut out = Vec::new();
            let indent_str = " ".repeat(n);
            let fmt = serde_json::ser::PrettyFormatter::with_indent(indent_str.as_bytes());
            let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
            serde::Serialize::serialize(&value, &mut ser).map_err(to_err)?;
            Ok(String::from_utf8(out).expect("serde_json writes UTF-8"))
        }
        _ => serde_json::to_string(&value).map_err(to_err),
    }
}

fn new_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .build()
            .expect("default syntax is valid"),
    );
    env.add_filter("tojson", tojson);
    env
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

impl PromptRegistry {
    /// An empty registry (no prompts loaded).
    pub fn empty() -> Self {
        Self { prompts: HashMap::new(), env: new_env() }
    }

    /// Load every `*.yaml` / `*.yml` file in `dir`. A missing directory loads nothing (logged);
    /// an invalid file, a template syntax error or a duplicate `name@version` is an error.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut registry = Self::empty();
        if !dir.is_dir() {
            tracing::warn!(dir = %dir.display(), "PROMPTS_DIR does not exist; no prompts loaded");
            return Ok(registry);
        }
        let mut files: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let spec: PromptSpec =
                serde_yaml::from_str(&text).with_context(|| format!("invalid prompt file {}", path.display()))?;
            registry.insert(spec).with_context(|| format!("invalid prompt file {}", path.display()))?;
        }
        tracing::info!(count = registry.len(), dir = %dir.display(), "loaded prompts");
        Ok(registry)
    }

    /// Add one prompt (compiles its template).
    pub fn insert(&mut self, spec: PromptSpec) -> Result<()> {
        anyhow::ensure!(valid_name(&spec.name), "prompt name {:?} must match [a-z0-9_.-]+", spec.name);
        let number = spec.version_number().with_context(|| format!("version {:?} must be v<digits>", spec.version))?;
        let versions = self.prompts.entry(spec.name.clone()).or_default();
        anyhow::ensure!(!versions.contains_key(&number), "duplicate prompt {}", spec.prompt_ref());
        self.env
            .add_template_owned(spec.prompt_ref(), spec.user_template.clone())
            .with_context(|| format!("template of {}", spec.prompt_ref()))?;
        versions.insert(number, spec);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.prompts.values().map(BTreeMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Prompt names, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.prompts.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    /// Loaded versions of `name`, oldest first.
    pub fn versions(&self, name: &str) -> Vec<&str> {
        self.prompts.get(name).map(|v| v.values().map(|p| p.version.as_str()).collect()).unwrap_or_default()
    }

    /// Prompt by name and version; `version = None` is the highest loaded version. `name` may also
    /// be a full `name@version` ref.
    pub fn get(&self, name: &str, version: Option<&str>) -> Result<&PromptSpec, PromptError> {
        let (name, version) = match name.split_once('@') {
            Some((n, v)) => (n, Some(version.unwrap_or(v))),
            None => (name, version),
        };
        let unknown = || PromptError::Unknown(version.map_or_else(|| name.to_string(), |v| format!("{name}@{v}")));
        let by_version = self.prompts.get(name).ok_or_else(unknown)?;
        match version {
            None => by_version.values().next_back().ok_or_else(unknown),
            Some(v) => {
                let n: u32 = v.strip_prefix('v').and_then(|n| n.parse().ok()).ok_or_else(unknown)?;
                by_version.get(&n).ok_or_else(unknown)
            }
        }
    }

    /// Render `user_template` with the packet `inputs` (a JSON object); `system` is returned as is.
    pub fn render(&self, prompt: &PromptSpec, inputs: &serde_json::Value) -> Result<RenderedPrompt, PromptError> {
        let prompt_ref = prompt.prompt_ref();
        let err = |source| PromptError::Render { prompt_ref: prompt_ref.clone(), source };
        let template = self.env.get_template(&prompt_ref).map_err(err)?;
        let user = template.render(minijinja::value::Serde(inputs)).map_err(err)?;
        Ok(RenderedPrompt { system: prompt.system.clone(), user })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "name: observer.code\nversion: v1\nsystem: You observe.\nuser_template: |\n  Events: {{ events | tojson }}\noutput_schema: observer.v1\n";
    const V2: &str = "name: observer.code\nversion: v2\nsystem: You observe v2.\nuser_template: \"{{ session.project_key }} <{{ title }}> {{ events | tojson(2) }}\"\noutput_schema: observer.v1\nmodel_hint: opus\n";

    #[test]
    fn load_get_render() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("observer.code.v1.yaml"), V1).unwrap();
        std::fs::write(dir.path().join("observer.code.v2.yml"), V2).unwrap();
        std::fs::write(dir.path().join("README.md"), "ignored").unwrap();
        let reg = PromptRegistry::load(dir.path()).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.names(), vec!["observer.code"]);
        assert_eq!(reg.versions("observer.code"), vec!["v1", "v2"]);
        assert_eq!(reg.get("observer.code", None).unwrap().version, "v2");
        assert_eq!(reg.get("observer.code@v1", None).unwrap().model_hint, "sonnet");
        assert_eq!(reg.get("observer.code", Some("v2")).unwrap().model_hint, "opus");
        assert!(matches!(reg.get("observer.code@v9", None), Err(PromptError::Unknown(_))));
        assert!(reg.get("nope", None).is_err());

        let p1 = reg.get("observer.code@v1", None).unwrap();
        let out = reg.render(p1, &serde_json::json!({"events": [{"tool": "<Edit>"}]})).unwrap();
        assert_eq!(out.system, "You observe.");
        assert_eq!(out.user, "Events: [{\"tool\":\"<Edit>\"}]\n");

        let p2 = reg.get("observer.code", None).unwrap();
        let inputs = serde_json::json!({"session": {"project_key": "github.com/a/b"}, "title": "a&b", "events": [1]});
        assert_eq!(reg.render(p2, &inputs).unwrap().user, "github.com/a/b <a&b> [\n  1\n]");
        // strict undefined
        let err = reg.render(p2, &serde_json::json!({"events": []})).unwrap_err();
        assert!(matches!(err, PromptError::Render { .. }), "{err}");
    }

    #[test]
    fn rejects_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), V1).unwrap();
        std::fs::write(dir.path().join("b.yaml"), V1).unwrap();
        assert!(PromptRegistry::load(dir.path()).unwrap_err().to_string().contains("b.yaml"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), V1.replace("observer.v1", "nope.v1")).unwrap();
        assert!(PromptRegistry::load(dir.path()).is_err());

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), V1.replace("{{ events | tojson }}", "{{ events ")).unwrap();
        assert!(PromptRegistry::load(dir.path()).is_err());

        assert!(PromptRegistry::load(Path::new("/nonexistent/prompts")).unwrap().is_empty());
    }
}
