//! Spec 9.4 result validation and idempotency; 10.4 / 10.5 appliers.

use std::time::Duration;

use axum::http::StatusCode;
use memvault_core::auth::scopes::TokenKind;
use memvault_core::estimate_tokens;
use memvault_core::invalidate::{CHANNEL, parse_payload};
use memvault_core::schemas::common::ScopeKind;
use memvault_core::test_support::{Seeded, seed_identity};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::common::{Ctx, PROJECT, ctx, observer_output, prompt, stop, tool};

/// A session whose first window (prompt, two tool uses, Stop) closed; returns (session, leased extract id).
async fn leased_extract(c: &Ctx, who: &Seeded) -> (Uuid, String) {
    let s = c.start_session(who, "cc").await;
    c.events_ok(&who.token, s, vec![prompt(s, 0, "fix the parser"), tool(s, 1), tool(s, 2), stop(s, 3, "Fixed.")])
        .await;
    let leased = c.lease(&who.token, json!({"session_id": s, "kinds": ["extract"]})).await;
    (s, leased[0]["id"].as_str().unwrap().to_string())
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Estimated tokens of the events `seqs` (every event when empty) of a packet's window, one JSON line each.
async fn window_tokens(pool: &PgPool, packet: &str, seqs: &[i64]) -> i32 {
    let inputs: Value = sqlx::query_scalar("SELECT inputs FROM work_packets WHERE id = $1::uuid")
        .bind(packet)
        .fetch_one(pool)
        .await
        .unwrap();
    inputs["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| seqs.is_empty() || seqs.contains(&e["seq"].as_i64().unwrap()))
        .map(|e| estimate_tokens(&e.to_string()))
        .sum()
}

#[sqlx::test(migrations = "../../migrations")]
async fn schema_invalid_output_gets_retry_text(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, id) = leased_extract(&c, &alice).await;

    let resp = c.result(&alice.token, &id, json!({"skip": "nope", "observations": []})).await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", resp.text());
    let err = &resp.json()["error"];
    assert_eq!(err["code"], "schema_invalid");
    let retry = err["retry_with"].as_str().unwrap();
    assert!(retry.contains("observer.v1"), "{retry}");
    assert!(retry.contains("fact_candidates") || retry.contains("/skip"), "{retry}");

    let resp = c.result(&alice.token, &id, json!("this is not json")).await;
    assert_eq!(resp.error_code().as_deref(), Some("schema_invalid"));

    // the lease is kept and the error recorded; a fenced valid output then applies
    let (status, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM work_packets WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "leased");
    assert!(last_error.unwrap().starts_with("schema_invalid"));
    let fenced = format!("```json\n{}\n```", observer_output("Parser drops last token", &[1, 2]));
    let resp = c.result(&alice.token, &id, Value::String(fenced)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["applied"]["observations"], 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn extract_result_applies_once(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (s, id) = leased_extract(&c, &alice).await;
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen(CHANNEL).await.unwrap();

    let mut output = observer_output("Parser drops the last token", &[1, 2]);
    output["fact_candidates"] = json!([{
        "subject": {"kind": "project", "name": "acme-app"}, "predicate": "project.stack.language",
        "value": "rust", "value_text": "Rust", "confidence": 0.8, "evidence_seqs": [2],
    }]);
    output["procedure_hints"] = json!([{"task_class": "debug-parser", "note": "check token boundaries first"}]);
    output["needs_escalation"] = json!(true);
    let resp = c.result(&alice.token, &id, output.clone()).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let applied = resp.json();
    assert_eq!(
        applied,
        json!({"applied": {"observations": 1, "duplicates": 0, "fact_candidates": 1, "fact_confirmations": 0,
                           "procedure_hints": 1}, "duplicate": false})
    );

    // observation: scope, provenance, source events
    let (obs_id, scope_kind, scope_key, sources, extracted, norm): (
        Uuid,
        String,
        String,
        Vec<Uuid>,
        Value,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT id, scope_kind::text, scope_key, source_event_ids, extracted_by, title_norm_key FROM observations",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((scope_kind.as_str(), scope_key.as_str()), ("project", PROJECT));
    let event_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM events WHERE session_id = $1 AND seq IN (1, 2) ORDER BY seq")
            .bind(s)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(sources, event_ids);
    assert_eq!(extracted["prompt"], "observer.code@v1");
    assert_eq!(extracted["runner"], "laptop");
    assert_eq!(extracted["packet_id"], id.as_str());
    assert_eq!(norm.unwrap().len(), 32);
    // work: the cited events as the window showed them to the extractor; the fact sums its evidence
    let expected = window_tokens(&pool, &id, &[1, 2]).await;
    assert!(expected > 0);
    let work: (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT (SELECT work_tokens FROM observations), (SELECT work_tokens FROM facts)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(work, (Some(expected), Some(expected)));

    // fact candidate with a new entity; hint; embed jobs; audit
    let (status, evidence, conflict): (String, Vec<Uuid>, bool) =
        sqlx::query_as("SELECT status, evidence_observation_ids, conflict FROM facts").fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), evidence, conflict), ("candidate", vec![obs_id], false));
    assert_eq!(count(&pool, "SELECT count(*) FROM entities WHERE canonical_name = 'acme-app'").await, 1);
    let hint_obs: Option<Uuid> =
        sqlx::query_scalar("SELECT observation_id FROM procedure_hints").fetch_one(&pool).await.unwrap();
    assert_eq!(hint_obs, Some(obs_id));
    let jobs: Vec<String> =
        sqlx::query_scalar("SELECT payload->>'owner_table' FROM jobs WHERE type = 'embed' ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(jobs, ["entities", "facts", "observations"]);
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM memory_audit ORDER BY action").fetch_all(&pool).await.unwrap();
    assert_eq!(actions, ["obs.create", "packet.needs_escalation"]);

    // the context cache is told about the project scope
    let note = tokio::time::timeout(Duration::from_secs(5), listener.recv()).await.unwrap().unwrap();
    let (space, kind, key) = parse_payload(note.payload()).unwrap();
    assert_eq!((space, kind, key.as_str()), (alice.space_id, ScopeKind::Project, PROJECT));

    // a duplicate post returns the first counts and changes nothing
    let resp = c.result(&alice.token, &id, output).await;
    assert_eq!(resp.status, StatusCode::OK);
    let again = resp.json();
    assert_eq!(again["applied"], applied["applied"]);
    assert_eq!(again["duplicate"], true);
    assert_eq!(count(&pool, "SELECT count(*) FROM observations").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM facts").await, 1);

    // the extract settled, so the Stop's prompt summary now exists, carrying the observation title
    let inputs: Value = sqlx::query_scalar("SELECT inputs FROM work_packets WHERE idempotency_key = $1")
        .bind(format!("summarize_prompt:{s}:1"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(inputs["prompt"], "fix the parser");
    assert_eq!(inputs["last_assistant_message"], "Fixed.");
    assert_eq!(inputs["observation_titles"][0]["title"], "Parser drops the last token");
}

#[sqlx::test(migrations = "../../migrations")]
async fn duplicate_observations_are_merged(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (s, first) = leased_extract(&c, &alice).await;
    assert_eq!(
        c.result(&alice.token, &first, observer_output("Parser drops the last token", &[1])).await.status,
        StatusCode::OK
    );

    // window 2: the same observation (content hash) with higher importance, and a near-identical title
    c.events_ok(&alice.token, s, vec![prompt(s, 4, "again"), tool(s, 5), stop(s, 6, "ok")]).await;
    let leased = c.lease(&alice.token, json!({"kinds": ["extract"]})).await;
    let mut output = observer_output("Parser drops the last token", &[5]);
    output["observations"][0]["importance"] = json!(0.9);
    let mut near = output["observations"][0].clone();
    near["title"] = json!("parser: drops THE last token!");
    near["facts"] = json!(["a different fact"]);
    output["observations"].as_array_mut().unwrap().push(near);
    let resp = c.result(&alice.token, leased[0]["id"].as_str().unwrap(), output).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["applied"]["duplicates"], 2);
    assert_eq!(resp.json()["applied"]["observations"], 0);
    let importance: f32 = sqlx::query_scalar("SELECT importance FROM observations").fetch_one(&pool).await.unwrap();
    assert!((importance - 0.9).abs() < 1e-6);
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit WHERE action = 'obs.merge'").await, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn fact_candidates_confirm_or_conflict(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (s, id) = leased_extract(&c, &alice).await;
    // an active fact about an entity known by an alias
    let entity: Uuid = sqlx::query_scalar(
        "INSERT INTO entities (id, space_id, kind, canonical_name, aliases) VALUES (gen_random_uuid(), $1, 'project', 'Acme App', '{acme-app}')
         RETURNING id",
    )
    .bind(alice.space_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let active: Uuid = sqlx::query_scalar(
        "INSERT INTO facts (id, space_id, scope_kind, scope_key, subject_id, predicate, value, value_text, confidence, status)
         VALUES (gen_random_uuid(), $1, 'project', $2, $3, 'project.stack.language', '\"rust\"', 'Rust', 0.6, 'active')
         RETURNING id",
    )
    .bind(alice.space_id)
    .bind(PROJECT)
    .bind(entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    let candidate = |value: &str, text: &str| {
        json!({"subject": {"kind": "project", "name": "ACME-APP"}, "predicate": "project.stack.language",
               "value": value, "value_text": text, "confidence": 0.8, "evidence_seqs": [1]})
    };
    let mut output = observer_output("Parser drops the last token", &[1]);
    output["fact_candidates"] = json!([candidate("rust", " rust "), candidate("go", "Go")]);
    let resp = c.result(&alice.token, &id, output).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["applied"]["fact_confirmations"], 1);
    assert_eq!(resp.json()["applied"]["fact_candidates"], 1);

    let (confirmations, confidence, learned): (i32, f32, Vec<Uuid>) =
        sqlx::query_as("SELECT confirmations, confidence, learned_on_client_ids FROM facts WHERE id = $1")
            .bind(active)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(confirmations, 2);
    assert!((f64::from(confidence) - (1.0 - 0.4 * 0.6)).abs() < 1e-5, "{confidence}");
    assert_eq!(learned, vec![alice.client_id]);
    let (conflict, supersedes, subject): (bool, Option<Uuid>, Uuid) =
        sqlx::query_as("SELECT conflict, supersedes_id, subject_id FROM facts WHERE status = 'candidate'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((conflict, supersedes, subject), (true, Some(active), entity));
    assert_eq!(count(&pool, "SELECT count(*) FROM entities").await, 1, "resolved through the alias");
    assert_eq!(count(&pool, "SELECT count(*) FROM memory_audit WHERE action = 'fact.confirm'").await, 1);
    let _ = s;
}

#[sqlx::test(migrations = "../../migrations")]
async fn skip_marks_done_without_rows(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, id) = leased_extract(&c, &alice).await;
    let resp = c
        .result(
            &alice.token,
            &id,
            json!({"skip": true, "skip_reason": "only reads", "observations": [], "fact_candidates": []}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["applied"]["observations"], 0);
    let result: Value = sqlx::query_scalar("SELECT result FROM work_packets WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(result["skip_reason"], "only reads");
    assert_eq!(count(&pool, "SELECT count(*) FROM observations").await, 0);
}

#[sqlx::test(migrations = "../../migrations")]
async fn prompt_and_session_summaries(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (s, id) = leased_extract(&c, &alice).await;
    let resp = c.result(&alice.token, &id, observer_output("Parser drops the last token", &[1])).await;
    assert_eq!(resp.status, StatusCode::OK);

    // end while the prompt summary is in flight: the session summary waits for it
    let resp = c.post(&format!("/v1/sessions/{s}/end"), &alice.token, json!({"reason": "exit"})).await;
    assert_eq!(resp.json()["packets_created"], json!([]));

    let p = c.lease(&alice.token, json!({"kinds": ["summarize_prompt"]})).await;
    assert_eq!(p[0]["prompt_ref"], "summary.prompt@v1");
    assert!(p[0]["prompt"]["user"].as_str().unwrap().starts_with("prompt 1: fix the parser"));
    let summary = json!({"request": "fix the parser", "learned": "last token dropped", "completed": "fixed",
                         "next_steps": "add a regression test"});
    let resp = c.result(&alice.token, p[0]["id"].as_str().unwrap(), summary).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.json()["applied"], json!({"summaries": 1}));

    let p = c.lease(&alice.token, json!({"kinds": ["summarize_session"]})).await;
    assert_eq!(p.len(), 1, "created once the prompt summary settled");
    assert_eq!((p[0]["prompt_ref"].clone(), p[0]["model_hint"].clone()), (json!("summary.session@v2"), json!("opus")));
    assert_eq!(p[0]["inputs"]["prompt_summaries"][0]["next_steps"], "add a regression test");
    let resp = c.result(&alice.token, p[0]["id"].as_str().unwrap(), json!({"next_steps": "ship it"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let rows: Vec<(String, Option<i32>, String, bool)> = sqlx::query_as(
        "SELECT grain, prompt_no, next_steps, period_start IS NOT NULL FROM summaries ORDER BY created_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("prompt".into(), Some(1), "add a regression test".into(), false),
            ("session".into(), None, "ship it".into(), true)
        ]
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs WHERE payload->>'owner_table' = 'summaries'").await, 2);
    let kinds: Vec<String> = c.packets(s).await.into_iter().map(|p| p.0).collect();
    assert_eq!(kinds, ["extract", "summarize_prompt", "summarize_session"]);

    // work: the prompt's one window and the whole session are the same four events here
    let window = window_tokens(&pool, &id, &[]).await;
    let work: Vec<Option<i32>> =
        sqlx::query_scalar("SELECT work_tokens FROM summaries ORDER BY created_at").fetch_all(&pool).await.unwrap();
    assert_eq!(work, [Some(window), Some(window)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn end_without_stop_still_summarises_the_last_prompt(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let s = c.start_session(&alice, "cc").await;
    c.events_ok(&alice.token, s, vec![prompt(s, 0, "fix the parser"), tool(s, 1), tool(s, 2)]).await;
    let resp = c.post(&format!("/v1/sessions/{s}/end"), &alice.token, json!({"reason": "interrupted"})).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let leased = c.lease(&alice.token, json!({"session_id": s, "kinds": ["extract"]})).await;
    let id = leased[0]["id"].as_str().unwrap();
    let resp = c.result(&alice.token, id, observer_output("Parser drops the last token", &[1])).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let p = c.lease(&alice.token, json!({"kinds": ["summarize_prompt"]})).await;
    assert_eq!(p.len(), 1, "owed although no Stop was seen");
    let summary = json!({"request": "fix the parser", "learned": "x", "completed": "y", "next_steps": "z"});
    let resp = c.result(&alice.token, p[0]["id"].as_str().unwrap(), summary).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    let kinds: Vec<String> = c.packets(s).await.into_iter().map(|p| p.0).collect();
    assert_eq!(kinds, ["extract", "summarize_prompt", "summarize_session"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn nul_in_model_output_is_stripped_not_a_500(pool: PgPool) {
    let c = ctx(pool.clone());
    let alice = seed_identity(&pool, TokenKind::Device).await;
    let (_, id) = leased_extract(&c, &alice).await;
    let resp = c.result(&alice.token, &id, observer_output("Parser drops\u{0} the last token", &[1, 2])).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let title: String = sqlx::query_scalar("SELECT title FROM observations").fetch_one(&pool).await.unwrap();
    assert_eq!(title, "Parser drops the last token");
}
