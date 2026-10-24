"""LLM output schemas named by ``work_packets.output_schema`` (spec 10.3, 10.5, 12.2, 12.3), code mode.

:func:`validate_output` implements spec 9.4: parse JSON, strip one surrounding code fence, validate
against the named model. Nothing else is "fixed" heuristically. Unknown extra keys are ignored.
"""

from __future__ import annotations

import json
import re
from datetime import date
from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, ValidationError, model_validator

from memsvc.schemas.common import Concept, ObservationType, Sensitivity

PREDICATE_PATTERN = r"^[a-z0-9_]+(\.[a-z0-9_]+)*$"


class _Out(BaseModel):
    model_config = ConfigDict(extra="ignore")


# ---------- observer.v1 (10.3) ----------


class ObservedObservation(_Out):
    type: ObservationType
    title: str = Field(min_length=1, max_length=120)
    subtitle: str | None = Field(None, max_length=200)
    narrative: str | None = Field(None, max_length=2000)
    facts: list[Annotated[str, Field(max_length=300)]] = Field(max_length=10)
    concepts: list[Concept] = Field(min_length=1, max_length=5)
    entities: list[str] = Field(default_factory=list)
    files: list[str] = Field(default_factory=list)
    importance: float = Field(ge=0, le=1)
    sensitivity: Sensitivity = "normal"
    source_event_seqs: list[int] = Field(min_length=1)
    # Not in the 10.3 JSON schema; 10.4 lets the prompt choose device scope for machine-level notes.
    scope_hint: Literal["global", "project", "device", "agent"] | None = None


class FactSubjectOut(_Out):
    kind: str = Field(min_length=1)
    name: str = Field(min_length=1)


class FactCandidate(_Out):
    subject: FactSubjectOut
    predicate: str = Field(pattern=PREDICATE_PATTERN)
    value: Any
    value_text: str = Field(max_length=300)
    confidence: float = Field(ge=0, le=1)
    scope_hint: Literal["global", "project", "device", "agent"] | None = None
    valid_from: str | None = None
    evidence_seqs: list[int]


class ProcedureHint(_Out):
    task_class: str = Field(min_length=1)
    note: str = Field(min_length=1)


class ObserverV1(_Out):
    skip: bool
    skip_reason: str | None = None
    observations: list[ObservedObservation] = Field(max_length=8)
    fact_candidates: list[FactCandidate] = Field(max_length=6)
    procedure_hints: list[ProcedureHint] = Field(default_factory=list)
    needs_escalation: bool = False


# ---------- summary.v1 (10.5) ----------

_SummaryText = Annotated[str, Field(max_length=600)]


class SummaryV1(_Out):
    request: _SummaryText | None = None
    investigated: _SummaryText | None = None
    learned: _SummaryText | None = None
    completed: _SummaryText | None = None
    next_steps: str = Field(min_length=1, max_length=600)  # mandatory, phrased as current trajectory
    notes: _SummaryText | None = None


# ---------- consolidate.facts.v1 (12.2) ----------
# Ids are strings exactly as given in the packet inputs (full UUIDs or the packet's own aliases).


class _Decision(_Out):
    reason: str = Field(min_length=1)


class Supersede(_Decision):
    action: Literal["supersede"]
    target_id: str
    by_candidate_id: str
    valid_from: str | None = None


class MergeInto(_Decision):
    action: Literal["merge_into"]
    target_id: str
    candidate_id: str


class KeepBoth(_Decision):
    action: Literal["keep_both"]
    ids: list[str] = Field(min_length=2)


class Retract(_Decision):
    action: Literal["retract"]
    target_id: str


class PromoteScope(_Decision):
    action: Literal["promote_scope"]
    target_id: str
    to: Literal["global", "project", "device", "agent"]


class SetConfidence(_Decision):
    action: Literal["set_confidence"]
    target_id: str
    confidence: float = Field(ge=0, le=1)


FactDecision = Annotated[
    Supersede | MergeInto | KeepBoth | Retract | PromoteScope | SetConfidence, Field(discriminator="action")
]


class ConsolidateFactsV1(_Out):
    decisions: list[FactDecision]


# ---------- consolidate.summary.v1 / consolidate.procedures.v1 (12.3) ----------


class Period(_Out):
    grain: Literal["day", "week"] | None = None
    start: date
    end: date


class ConsolidateSummaryV1(SummaryV1):
    period: Period


class ConsolidateProceduresV1(_Out):
    """One procedure (new, or a revision when ``supersedes_id`` is set). ``skip`` (not in the spec)
    lets the model decline when the hints do not describe a reusable procedure."""

    skip: bool = False
    skip_reason: str | None = None
    name: str | None = Field(None, min_length=1, max_length=200)
    task_class: str | None = Field(None, min_length=1)
    trigger_text: str | None = Field(None, min_length=1)
    body_md: str | None = Field(None, min_length=1)
    supersedes_id: str | None = None
    confidence: float = Field(0.6, ge=0, le=1)

    @model_validator(mode="after")
    def _required_unless_skip(self) -> ConsolidateProceduresV1:
        if not self.skip:
            missing = [f for f in ("name", "task_class", "trigger_text", "body_md") if not getattr(self, f)]
            if missing:
                raise ValueError(f"missing required fields: {', '.join(missing)}")
        return self


OUTPUT_SCHEMAS: dict[str, type[BaseModel]] = {
    "observer.v1": ObserverV1,
    "summary.v1": SummaryV1,
    "consolidate.facts.v1": ConsolidateFactsV1,
    "consolidate.summary.v1": ConsolidateSummaryV1,
    "consolidate.procedures.v1": ConsolidateProceduresV1,
}

_FENCE_RE = re.compile(r"^\s*```[A-Za-z0-9_-]*\s*\n(.*?)\n?\s*```\s*$", re.DOTALL)


class OutputInvalid(ValueError):
    """Raised by :func:`validate_output`; ``str(exc)`` is the text to return as ``retry_with``."""


def strip_code_fence(text: str) -> str:
    m = _FENCE_RE.match(text)
    return m.group(1) if m else text.strip()


def validate_output(schema_name: str, output: dict[str, Any] | str) -> BaseModel:
    """Parse and validate a runner's output against ``OUTPUT_SCHEMAS[schema_name]``."""
    model = OUTPUT_SCHEMAS.get(schema_name)
    if model is None:
        raise KeyError(f"unknown output schema {schema_name!r}")
    data: Any = output
    if isinstance(output, str):
        try:
            data = json.loads(strip_code_fence(output))
        except json.JSONDecodeError as e:
            raise OutputInvalid(f"output is not valid JSON: {e}") from e
    if not isinstance(data, dict):
        raise OutputInvalid("output must be a JSON object")
    try:
        return model.model_validate(data)
    except ValidationError as e:
        raise OutputInvalid(f"output does not match {schema_name}: {e}") from e
