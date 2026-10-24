//! Replay, redact and record against the example eval set in `evals/`.

use std::path::PathBuf;

use memvault_core::test_support::test_state;
use memvault_evals::record::record;
use memvault_evals::redact::{redact_str, redact_text};
use memvault_evals::replay::{Baseline, EvalSet, Observer, ReplayOptions, baseline_regression, replay, state_over};
use sqlx::PgPool;

fn evals_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals")
}

#[sqlx::test(migrations = "../../migrations")]
async fn replays_the_example_set(pool: PgPool) {
    let base = test_state(pool.clone());
    let state = state_over(&base, base.db.clone()).unwrap();
    let set = EvalSet::load(&evals_dir()).unwrap();
    assert_eq!(set.sessions.len(), 2);

    let report = replay(&state, &set, &ReplayOptions::default()).await.unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_eq!(report.questions, 3);
    assert_eq!(report.packets_failed, 0);
    assert_eq!(report.recall_at_5, 1.0, "{report:?}");
    assert!(report.first_hit_rank.unwrap() >= 1.0);
    assert!(report.tokens_per_hit.unwrap() > 0.0);
    assert_eq!(report.fact_precision, Some(1.0));
    assert_eq!(report.facts_extracted, 1);
    assert_eq!(report.contradictions, 0);
    assert!(baseline_regression(&report, set.baseline.as_ref().unwrap()).is_none());
    assert!(baseline_regression(&report, &Baseline { recall_at_5: 1.0 }).is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn fake_observer_replays_without_recorded_outputs(pool: PgPool) {
    let base = test_state(pool.clone());
    let state = state_over(&base, base.db.clone()).unwrap();
    let set = EvalSet::load(&evals_dir()).unwrap();
    let opts = ReplayOptions { observer: Observer::Fake, rerank: false };
    let report = replay(&state, &set, &opts).await.unwrap();
    assert_eq!(report.packets_failed, 0);
    // The fake observer titles an observation with the prompt, so the CI question still hits.
    let ci = report.per_question.iter().find(|q| q.id == "ci-cache").unwrap();
    assert_eq!(ci.recall_at_5, 1.0);
    assert_eq!(report.facts_extracted, 0);
    assert_eq!(report.fact_precision, Some(0.0));
}

#[test]
fn a_recall_drop_over_five_percent_fails_the_baseline() {
    let mut report = memvault_evals::replay::Report {
        questions: 1,
        recall_at_5: 0.96,
        first_hit_rank: None,
        tokens_per_hit: None,
        fact_precision: None,
        facts_extracted: 0,
        contradictions: 0,
        packets_failed: 0,
        per_question: vec![],
    };
    let baseline = Baseline { recall_at_5: 1.0 };
    assert!(baseline_regression(&report, &baseline).is_none());
    report.recall_at_5 = 0.9;
    assert!(baseline_regression(&report, &baseline).unwrap().contains("below the baseline"));
}

#[test]
fn redact_masks_secrets_emails_tokens_and_home_paths() {
    let token = format!("mem_{}_{}", "abcd1234", "A".repeat(32));
    let input = format!(
        "mail alex@example.com, token {token}, key sk-abcdefghijklmnopqrstuv, file /home/alex/src/app/x.rs \
         and /Users/bob/y, C:\\Users\\carol\\z, Authorization: Bearer abcdefghijklmnop1234, password=hunter2-x9y8, \
         ghp_0123456789abcdefghijABCDEFGHIJ012345, AKIAABCDEFGHIJKLMNOP"
    );
    let out = redact_str(&input);
    for leaked in [
        "alex@example.com",
        &token,
        "sk-abcdef",
        "alex",
        "bob",
        "carol",
        "abcdefghijklmnop1234",
        "hunter2",
        "ghp_0123",
        "AKIAABCD",
    ] {
        assert!(!out.contains(leaked), "{leaked} leaked: {out}");
    }
    assert!(out.contains("~/src/app/x.rs"), "{out}");
    assert_eq!(redact_str(&out), out, "redaction is idempotent");
    // prose is left alone
    assert_eq!(redact_str("the token: parser reads one token"), "the token: parser reads one token");

    let line = serde_json::json!({
        "type": "event", "payload": {"api_key": "plain", "command": "cat /root/.ssh/id", "n": 3},
    });
    let out = redact_text(&format!("{line}\n")).unwrap();
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["payload"]["api_key"], "<secret>");
    assert_eq!(v["payload"]["command"], "cat ~/.ssh/id");
    assert_eq!(v["payload"]["n"], 3);
}

#[sqlx::test(migrations = "../../migrations")]
async fn record_exports_a_replayed_session(pool: PgPool) {
    let base = test_state(pool.clone());
    let state = state_over(&base, base.db.clone()).unwrap();
    let set = EvalSet::load(&evals_dir()).unwrap();
    replay(&state, &set, &ReplayOptions::default()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = record(&state, "example-parser-fix", dir.path()).await.unwrap();
    let recorded = memvault_evals::session::load(&path).unwrap();
    let original = &set.sessions[0].events;
    assert_eq!(recorded.external_session_id, "example-parser-fix");
    assert_eq!(recorded.events.len(), original.len());
    assert!(recorded.outputs.len() >= 2, "extract and summaries were recorded");

    // The recording replays to the same result.
    let again = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(again.path().join("sessions")).unwrap();
    std::fs::copy(&path, again.path().join("sessions/s.jsonl")).unwrap();
    std::fs::write(
        again.path().join("questions.yaml"),
        "questions:\n  - id: q\n    query: why does the parser drop the last token\n    expect:\n      - observation: parser drops the last token\n",
    )
    .unwrap();
    let set = EvalSet::load(again.path()).unwrap();
    let report = replay(&state, &set, &ReplayOptions::default()).await.unwrap();
    assert_eq!(report.recall_at_5, 1.0, "{report:?}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn cli_replay_uses_and_drops_a_scratch_database(pool: PgPool) {
    let state = test_state(pool.clone());
    let dir = evals_dir().display().to_string();
    memvault_evals::cli::run(state.clone(), vec!["replay".into(), "--dir".into(), dir.clone()]).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_database WHERE datname LIKE 'memvault_eval_%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);

    // A baseline above what the replay reaches makes the command fail.
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&evals_dir(), tmp.path());
    std::fs::write(tmp.path().join("baseline.json"), r#"{"recall_at_5": 2.0}"#).unwrap();
    let args = vec!["replay".into(), "--dir".into(), tmp.path().display().to_string()];
    let err = memvault_evals::cli::run(state, args).await.unwrap_err();
    assert!(err.to_string().contains("below the baseline"), "{err}");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn compare_reads_claude_mem_and_this_service(pool: PgPool) {
    let base = test_state(pool.clone());
    let state = state_over(&base, base.db.clone()).unwrap();
    let set = EvalSet::load(&evals_dir()).unwrap();
    replay(&state, &set, &ReplayOptions::default()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude-mem.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE sdk_sessions (content_session_id TEXT, memory_session_id TEXT);
         CREATE TABLE observations (memory_session_id TEXT, title TEXT, narrative TEXT);
         CREATE TABLE session_summaries (memory_session_id TEXT, request TEXT);
         INSERT INTO sdk_sessions VALUES ('example-parser-fix', 'mem-1');
         INSERT INTO observations VALUES ('mem-1', 'Parser bug', 'The parser skipped the last token of the input');
         INSERT INTO observations VALUES ('mem-1', 'Second', 'Another one');
         INSERT INTO session_summaries VALUES ('mem-1', 'fix it');",
    )
    .unwrap();
    drop(db);

    let rows = memvault_evals::compare::compare(&state, &path, &["example-parser-fix".into(), "unknown".into()])
        .await
        .unwrap();
    assert_eq!(rows[0].claude_mem.observations, 2);
    assert_eq!(rows[0].claude_mem.summaries, 1);
    assert!(rows[0].claude_mem.observation_tokens > 0);
    assert_eq!(rows[0].memvault.observations, 1);
    assert_eq!(rows[0].memvault.summaries, 1);
    assert_eq!(rows[0].memvault_facts, 1);
    assert_eq!(rows[1].claude_mem.observations + rows[1].memvault.observations, 0);
}
