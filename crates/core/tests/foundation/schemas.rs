use memvault_core::schemas::common::{OutputSchemaName, Scope};
use memvault_core::schemas::memories::{GetRequest, MemoryItem, RememberRequest, SearchQuery};
use memvault_core::schemas::outputs::{FactDecision, LlmOutput};
use memvault_core::schemas::recall::{RecallFormat, RecallRequest};
use memvault_core::schemas::validate::{OutputError, OutputSchemas, strip_code_fence};
use memvault_core::schemas::work::LeaseRequest;
use memvault_core::schemas::work_state::WorkStateQuery;
use memvault_core::test_support::repo_dir;
use serde_json::{Value, json};

fn schemas() -> OutputSchemas {
    OutputSchemas::load(&repo_dir().join("schemas")).unwrap()
}

fn observer_ok() -> Value {
    json!({
        "skip": false,
        "observations": [{
            "type": "decision",
            "title": "Chose pgvector over Chroma",
            "narrative": "One store, RLS covers vectors.",
            "facts": ["pgvector 0.6 supports HNSW"],
            "concepts": ["trade-off"],
            "files": ["migrations/0008_embeddings.sql"],
            "importance": 0.8,
            "source_event_seqs": [12, 13]
        }],
        "fact_candidates": [{
            "subject": {"kind": "project", "name": "memoryvault"},
            "predicate": "project.stack.vector_store",
            "value": "pgvector",
            "value_text": "uses pgvector",
            "confidence": 0.7,
            "evidence_seqs": [12]
        }],
        "procedure_hints": [{"task_class": "deploy", "note": "run migrations first"}],
        "extra_key_is_ignored": true
    })
}

#[test]
fn observer_roundtrip_and_fences() {
    let s = schemas();
    let out = s.validate(OutputSchemaName::ObserverV1, &observer_ok()).unwrap();
    let LlmOutput::Observer(obs) = &out else { panic!("wrong variant") };
    assert_eq!(obs.observations[0].source_event_seqs, vec![12, 13]);
    let again = s.validate(OutputSchemaName::ObserverV1, &serde_json::to_value(&out).unwrap()).unwrap();
    assert_eq!(again, out);
    let fenced = format!("```json\n{}\n```", observer_ok());
    assert_eq!(s.validate(OutputSchemaName::ObserverV1, &Value::String(fenced)).unwrap(), out);
    let bare_fence = format!("  ```\n{}\n```  ", observer_ok());
    assert_eq!(s.validate(OutputSchemaName::ObserverV1, &Value::String(bare_fence)).unwrap(), out);
    assert_eq!(strip_code_fence("  {\"a\": 1} "), "{\"a\": 1}");
}

#[test]
fn observer_rejects() {
    let s = schemas();
    type Mutation = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Mutation)> = vec![
        ("type", Box::new(|d| d["observations"][0]["type"] = json!("opinion"))),
        ("title", Box::new(|d| d["observations"][0]["title"] = json!("x".repeat(121)))),
        ("concepts", Box::new(|d| d["observations"][0]["concepts"] = json!([]))),
        ("source_event_seqs", Box::new(|d| d["observations"][0]["source_event_seqs"] = json!([]))),
        ("predicate", Box::new(|d| d["fact_candidates"][0]["predicate"] = json!("Prefers Editor"))),
        (
            "fact_candidates",
            Box::new(|d| {
                d.as_object_mut().unwrap().remove("fact_candidates");
            }),
        ),
        (
            "observations",
            Box::new(|d| {
                let o = d["observations"][0].clone();
                d["observations"] = json!(vec![o; 9]);
            }),
        ),
    ];
    for (what, mutate) in cases {
        let mut data = observer_ok();
        mutate(&mut data);
        let err = s.validate(OutputSchemaName::ObserverV1, &data).unwrap_err();
        let OutputError::Invalid(text) = err else { panic!("{what}: wrong error") };
        assert!(text.contains("observer.v1"), "{what}: {text}");
    }
}

#[test]
fn invalid_json_and_unknown_schema() {
    let s = schemas();
    for bad in [json!("not json"), json!("[1, 2]"), json!([1, 2])] {
        assert!(matches!(s.validate(OutputSchemaName::SummaryV1, &bad), Err(OutputError::Invalid(_))), "{bad}");
    }
    assert!(matches!(s.validate_value("nope.v1", &json!({})), Err(OutputError::UnknownSchema(_))));
    let mut names: Vec<_> = s.names().collect();
    names.sort();
    assert_eq!(
        names,
        ["consolidate.facts.v1", "consolidate.procedures.v1", "consolidate.summary.v1", "observer.v1", "summary.v1"]
    );
}

#[test]
fn summary_and_consolidation_schemas() {
    let s = schemas();
    assert!(matches!(
        s.validate(OutputSchemaName::SummaryV1, &json!({"next_steps": "wire TEI"})),
        Ok(LlmOutput::Summary(_))
    ));
    assert!(s.validate(OutputSchemaName::SummaryV1, &json!({"request": "x"})).is_err());
    assert!(s.validate(OutputSchemaName::SummaryV1, &json!({"next_steps": "x".repeat(601)})).is_err());
    let ok = s.validate(
        OutputSchemaName::ConsolidateSummaryV1,
        &json!({"next_steps": "n", "period": {"grain": "day", "start": "2026-10-01", "end": "2026-10-01"}}),
    );
    assert!(matches!(ok, Ok(LlmOutput::ConsolidateSummary(_))), "{ok:?}");
    assert!(
        s.validate(OutputSchemaName::ConsolidateSummaryV1, &json!({"next_steps": "n", "period": {"start": "Oct 1"}}))
            .is_err()
    );

    let facts = s
        .validate(
            OutputSchemaName::ConsolidateFactsV1,
            &json!({"decisions": [
                {"action": "supersede", "target_id": "a", "by_candidate_id": "b", "valid_from": "2026-10-01", "reason": "r"},
                {"action": "merge_into", "target_id": "a", "candidate_id": "c", "reason": "r"},
                {"action": "keep_both", "ids": ["a", "b"], "reason": "r"},
                {"action": "retract", "target_id": "a", "reason": "r"},
                {"action": "promote_scope", "target_id": "a", "to": "global", "reason": "r"},
                {"action": "set_confidence", "target_id": "a", "confidence": 0.9, "reason": "r"}
            ]}),
        )
        .unwrap();
    let LlmOutput::ConsolidateFacts(f) = facts else { panic!() };
    assert!(matches!(f.decisions.last(), Some(FactDecision::SetConfidence { confidence, .. }) if *confidence == 0.9));
    for bad in [
        json!({"decisions": [{"action": "explode", "reason": "r"}]}),
        json!({"decisions": [{"action": "keep_both", "ids": ["a"], "reason": "r"}]}),
        json!({"decisions": [{"action": "retract", "reason": "r"}]}),
        json!({"decisions": [{"action": "set_confidence", "target_id": "a", "confidence": 2, "reason": "r"}]}),
    ] {
        assert!(s.validate(OutputSchemaName::ConsolidateFactsV1, &bad).is_err(), "{bad}");
    }
    assert!(matches!(
        s.validate(OutputSchemaName::ConsolidateProceduresV1, &json!({"skip": true, "skip_reason": "too few"})),
        Ok(LlmOutput::ConsolidateProcedures(p)) if p.skip
    ));
    assert!(s.validate(OutputSchemaName::ConsolidateProceduresV1, &json!({"name": "Deploy"})).is_err());
    let p = s
        .validate(
            OutputSchemaName::ConsolidateProceduresV1,
            &json!({"name": "Deploy", "task_class": "deploy", "trigger_text": "when deploying", "body_md": "1. migrate"}),
        )
        .unwrap();
    assert!(matches!(p, LlmOutput::ConsolidateProcedures(p) if p.confidence == 0.6 && !p.skip));
}

#[test]
fn api_models() {
    let req: RecallRequest =
        serde_json::from_value(json!({"query": "redeploy", "scopes": [{"kind": "global", "key": ""}]})).unwrap();
    assert_eq!(req.budgets.observations, 600);
    assert_eq!(req.exclude_sensitivity, vec![memvault_core::schemas::common::Sensitivity::Secret]);
    assert_eq!(req.format, RecallFormat::Both);
    assert!(req.rerank && req.expand_top_observations == 3);

    let q: SearchQuery = serde_urlencoded::from_str("").unwrap();
    assert!(q.validate().is_err());
    let q: SearchQuery =
        serde_urlencoded::from_str("types=decision,bugfix&scopes=global:,project:github.com/a/b").unwrap();
    q.validate().unwrap();
    assert_eq!(q.limit, 20);
    assert_eq!(q.types.as_deref(), Some(&["decision".to_string(), "bugfix".to_string()][..]));
    assert_eq!(q.scopes.unwrap()[1], "project:github.com/a/b".parse::<Scope>().unwrap());
    let q: SearchQuery = serde_urlencoded::from_str("q=x&limit=500").unwrap();
    assert!(q.validate().is_err());
    let q: SearchQuery = serde_urlencoded::from_str("as_of=2026-10-01T12:00:00Z").unwrap();
    q.validate().unwrap();
    assert_eq!(q.as_of.unwrap().to_rfc3339(), "2026-10-01T12:00:00+00:00");
    assert!(serde_urlencoded::from_str::<SearchQuery>("q=x&as_of=last-tuesday").is_err());
    let req: RecallRequest = serde_json::from_value(json!({"as_of": "2026-10-01T12:00:00Z"})).unwrap();
    assert!(req.as_of.is_some());

    assert!(GetRequest { ids: vec![] }.validate().is_err());
    assert!(GetRequest { ids: vec![uuid::Uuid::nil(); 21] }.validate().is_err());

    let remember: RememberRequest = serde_json::from_value(json!({
        "kind": "fact", "subject": {"name": "alex"}, "predicate": "prefers.editor", "value": "helix", "value_text": "helix"
    }))
    .unwrap();
    remember.validate().unwrap();
    let RememberRequest::Fact(f) = &remember else { panic!() };
    assert_eq!(f.subject.kind, "user");
    let bad: RememberRequest = serde_json::from_value(json!({
        "kind": "fact", "subject": {"name": "alex"}, "predicate": "Prefers Editor", "value_text": "helix"
    }))
    .unwrap();
    assert!(bad.validate().is_err());

    let item: MemoryItem = serde_json::from_value(json!({
        "table": "procedures", "id": "0190c1b2-0000-7000-8000-000000000000",
        "space_id": "0190c1b2-0000-7000-8000-000000000001", "scope": {"kind": "global"}, "name": "n",
        "task_class": "t", "trigger_text": "x", "body_md": "b", "created_at": "2026-10-24T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(item.table(), memvault_core::schemas::common::MemoryTable::Procedures);
    assert_eq!(serde_json::to_value(&item).unwrap()["table"], "procedures");

    let ws: WorkStateQuery = serde_urlencoded::from_str("list=todo&scope=project:x").unwrap();
    assert_eq!(ws.list_name.as_deref(), Some("todo"));
    assert_eq!(ws.scope.unwrap().key, "x");

    let lease: LeaseRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!((lease.max, lease.lease_seconds, lease.kinds.len()), (1, 600, 3));
    assert!(LeaseRequest { max: 51, ..lease }.validate().is_err());
}
