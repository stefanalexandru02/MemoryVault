//! The repository's `prompts/` load, name existing output schemas, and carry the rules code relies on.

use memvault_core::prompts::PromptRegistry;
use memvault_core::schemas::validate::OutputSchemas;
use memvault_core::test_support::repo_dir;

#[test]
fn repository_prompts_load_with_their_schemas() {
    let prompts = PromptRegistry::load(&repo_dir().join("prompts")).expect("loading prompts/");
    let schemas = OutputSchemas::load(&repo_dir().join("schemas")).expect("loading schemas/");
    let schema_names: Vec<&str> = schemas.names().collect();
    for name in [
        "observer.code",
        "summary.prompt",
        "summary.session",
        "consolidate.facts",
        "consolidate.summary",
        "consolidate.procedures",
    ] {
        let spec = prompts.get(name, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        let schema = serde_json::to_value(spec.output_schema).unwrap();
        assert!(schema_names.contains(&schema.as_str().unwrap()), "{name}: schema {schema} not in schemas/");
    }
    let observer = prompts.get("observer.code", None).unwrap();
    assert!(observer.system.contains("subagent:<agent_type>"), "observer.code lost the SUBAGENT TAGGING rule");
}
