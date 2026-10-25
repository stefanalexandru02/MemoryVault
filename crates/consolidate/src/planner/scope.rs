//! Which scopes of a space one planner pass covers: a single scope (a triggered run), or every scope
//! except those planned moments ago (the nightly pass).

use memvault_core::schemas::common::{Scope, ScopeKind};

#[derive(Debug, Clone, Default)]
pub struct ScopeFilter {
    only: Option<Scope>,
    /// `kind:key` of the scopes left out.
    skip: Vec<String>,
}

impl ScopeFilter {
    pub fn only(scope: Scope) -> Self {
        Self { only: Some(scope), skip: Vec::new() }
    }

    pub fn skipping(scopes: &[Scope]) -> Self {
        Self { only: None, skip: scopes.iter().map(Scope::as_text).collect() }
    }

    pub fn allows(&self, kind: ScopeKind, key: &str) -> bool {
        self.only.as_ref().is_none_or(|s| s.kind == kind && s.key == key)
            && !self.skip.contains(&Scope::new(kind, key).as_text())
    }

    /// The scope a pass is restricted to, `None` when it covers the space.
    pub fn restricted_to(&self) -> Option<&Scope> {
        self.only.as_ref()
    }

    /// SQL condition on a table's scope columns. It takes three binds from `$first` on, in this order:
    /// [`Self::only_kind`], [`Self::only_key`], [`Self::skipped`]. The scope tests come first so an index on
    /// `(space_id, scope_kind, scope_key, ...)` serves a single-scope pass.
    pub fn condition(kind_col: &str, key_col: &str, first: usize) -> String {
        let (kind, key, skipped) = (first, first + 1, first + 2);
        format!(
            "(${kind}::scope_kind IS NULL OR ({kind_col} = ${kind} AND {key_col} = ${key}))
             AND NOT (({kind_col}::text || ':' || {key_col}) = ANY(${skipped}::text[]))"
        )
    }

    pub fn only_kind(&self) -> Option<ScopeKind> {
        self.only.as_ref().map(|s| s.kind)
    }

    pub fn only_key(&self) -> Option<&str> {
        self.only.as_ref().map(|s| s.key.as_str())
    }

    pub fn skipped(&self) -> &[String] {
        &self.skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allows_exactly_one_scope() {
        let f = ScopeFilter::only(Scope::new(ScopeKind::Project, "a"));
        assert!(f.allows(ScopeKind::Project, "a"));
        assert!(!f.allows(ScopeKind::Project, "b"));
        assert!(!f.allows(ScopeKind::Global, "a"));
    }

    #[test]
    fn skipping_allows_everything_else() {
        let f = ScopeFilter::skipping(&[Scope::new(ScopeKind::Project, "a"), Scope::global()]);
        assert!(!f.allows(ScopeKind::Project, "a"));
        assert!(!f.allows(ScopeKind::Global, ""));
        assert!(f.allows(ScopeKind::Project, "b"));
        assert!(ScopeFilter::default().allows(ScopeKind::Device, "x"));
    }
}
