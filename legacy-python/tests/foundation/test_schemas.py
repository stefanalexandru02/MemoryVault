from __future__ import annotations

import json

import pytest
from pydantic import TypeAdapter, ValidationError

from memsvc.schemas.common import Scope, estimate_tokens, parse_scopes_param
from memsvc.schemas.memories import GetRequest, MemoryItem, RememberRequest, SearchQuery
from memsvc.schemas.outputs import (
    ConsolidateFactsV1,
    ConsolidateProceduresV1,
    ConsolidateSummaryV1,
    ObserverV1,
    OutputInvalid,
    SummaryV1,
    strip_code_fence,
    validate_output,
)
from memsvc.schemas.recall import RecallRequest
from memsvc.schemas.work_state import WorkStateQuery

OBSERVER_OK = {
    "skip": False,
    "observations": [
        {
            "type": "decision",
            "title": "Chose pgvector over Chroma",
            "narrative": "One store, RLS covers vectors.",
            "facts": ["pgvector 0.6 supports HNSW"],
            "concepts": ["trade-off"],
            "files": ["server/migrations/0008_embeddings.sql"],
            "importance": 0.8,
            "source_event_seqs": [12, 13],
        }
    ],
    "fact_candidates": [
        {
            "subject": {"kind": "project", "name": "memoryvault"},
            "predicate": "project.stack.vector_store",
            "value": "pgvector",
            "value_text": "uses pgvector",
            "confidence": 0.7,
            "evidence_seqs": [12],
        }
    ],
    "procedure_hints": [{"task_class": "deploy", "note": "run migrations first"}],
}


def test_observer_roundtrip_and_fences() -> None:
    model = validate_output("observer.v1", OBSERVER_OK)
    assert isinstance(model, ObserverV1)
    again = ObserverV1.model_validate_json(model.model_dump_json())
    assert again == model
    fenced = "```json\n" + json.dumps(OBSERVER_OK) + "\n```"
    assert validate_output("observer.v1", fenced) == model
    assert strip_code_fence('  {"a": 1} ') == '{"a": 1}'


@pytest.mark.parametrize(
    "mutate",
    [
        lambda d: d["observations"][0].update(type="opinion"),
        lambda d: d["observations"][0].update(title="x" * 121),
        lambda d: d["observations"][0].update(concepts=[]),
        lambda d: d["observations"][0].update(source_event_seqs=[]),
        lambda d: d["fact_candidates"][0].update(predicate="Prefers Editor"),
        lambda d: d.pop("fact_candidates"),
        lambda d: d.update(observations=[d["observations"][0]] * 9),
    ],
)
def test_observer_rejects(mutate) -> None:  # type: ignore[no-untyped-def]
    data = json.loads(json.dumps(OBSERVER_OK))
    mutate(data)
    with pytest.raises(OutputInvalid) as e:
        validate_output("observer.v1", data)
    assert "observer.v1" in str(e.value)


def test_output_invalid_json_and_unknown_schema() -> None:
    with pytest.raises(OutputInvalid):
        validate_output("summary.v1", "not json")
    with pytest.raises(OutputInvalid):
        validate_output("summary.v1", "[1, 2]")
    with pytest.raises(KeyError):
        validate_output("nope.v1", {})


def test_summary_and_consolidation_schemas() -> None:
    assert isinstance(validate_output("summary.v1", {"next_steps": "wire TEI"}), SummaryV1)
    with pytest.raises(OutputInvalid):
        validate_output("summary.v1", {"request": "x"})
    with pytest.raises(OutputInvalid):
        validate_output("summary.v1", {"next_steps": "x" * 601})
    s = validate_output(
        "consolidate.summary.v1",
        {"next_steps": "n", "period": {"grain": "day", "start": "2026-10-01", "end": "2026-10-01"}},
    )
    assert isinstance(s, ConsolidateSummaryV1)
    f = validate_output(
        "consolidate.facts.v1",
        {
            "decisions": [
                {
                    "action": "supersede",
                    "target_id": "a",
                    "by_candidate_id": "b",
                    "valid_from": "2026-10-01",
                    "reason": "r",
                },
                {"action": "merge_into", "target_id": "a", "candidate_id": "c", "reason": "r"},
                {"action": "keep_both", "ids": ["a", "b"], "reason": "r"},
                {"action": "retract", "target_id": "a", "reason": "r"},
                {"action": "promote_scope", "target_id": "a", "to": "global", "reason": "r"},
                {"action": "set_confidence", "target_id": "a", "confidence": 0.9, "reason": "r"},
            ]
        },
    )
    assert isinstance(f, ConsolidateFactsV1)
    assert [d.action for d in f.decisions][-1] == "set_confidence"
    with pytest.raises(OutputInvalid):
        validate_output("consolidate.facts.v1", {"decisions": [{"action": "explode", "reason": "r"}]})
    p = validate_output("consolidate.procedures.v1", {"skip": True, "skip_reason": "too few"})
    assert isinstance(p, ConsolidateProceduresV1)
    with pytest.raises(OutputInvalid):
        validate_output("consolidate.procedures.v1", {"name": "Deploy"})


def test_api_models() -> None:
    req = RecallRequest.model_validate({"query": "redeploy", "scopes": [{"kind": "global", "key": ""}]})
    assert req.budgets.observations == 600 and req.exclude_sensitivity == ["secret"] and req.format == "both"
    with pytest.raises(ValidationError):
        SearchQuery()
    assert SearchQuery(types=["decision"]).limit == 20
    with pytest.raises(ValidationError):
        GetRequest(ids=[])
    remember = TypeAdapter(RememberRequest).validate_python(
        {
            "kind": "fact",
            "subject": {"name": "alex"},
            "predicate": "prefers.editor",
            "value": "helix",
            "value_text": "helix",
        }
    )
    assert remember.kind == "fact"
    item = TypeAdapter(MemoryItem).validate_python(
        {"table": "procedures", "id": "0190c1b2-0000-7000-8000-000000000000", "space_id":
         "0190c1b2-0000-7000-8000-000000000001", "scope": {"kind": "global"}, "name": "n", "task_class": "t",
         "trigger_text": "x", "body_md": "b", "created_at": "2026-10-24T00:00:00Z"}
    )  # fmt: skip
    assert item.table == "procedures"
    assert WorkStateQuery.model_validate({"list": "todo"}).list_name == "todo"


def test_scope_helpers() -> None:
    assert Scope.parse("project:github.com/a/b").key == "github.com/a/b"
    assert Scope(kind="global").as_text() == "global:"
    assert parse_scopes_param("global:,device:laptop") == [Scope(kind="global"), Scope(kind="device", key="laptop")]
    assert estimate_tokens("abcde") == 2 and estimate_tokens("") == 0
