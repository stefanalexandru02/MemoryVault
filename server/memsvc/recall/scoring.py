"""Pure ranking math for the recall engine (spec 11.2-11.5): RRF, recency, final score, MMR, budget fill."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, TypeVar
from uuid import UUID

import numpy as np

RRF_K = 60
MMR_LAMBDA = 0.7
INTENT_BONUS = 0.3  # procedures whose task_class equals task_state.intent (11.2)

# Half-lives in days (11.4). Facts and procedures have no recency decay.
HALF_LIFE_OBS_DEFAULT = 30.0
HALF_LIFE_OBS_LONG = 90.0  # decision, bugfix
HALF_LIFE_SUMMARIES = 60.0
LONG_LIVED_TYPES = frozenset({"decision", "bugfix"})

# Type boosts (11.4 leaves the values open): decisions, bug fixes and security alerts are the
# observations most often worth surfacing again.
TYPE_BOOST: dict[str, float] = {"decision": 1.2, "bugfix": 1.1, "security_alert": 1.2}


@dataclass
class Candidate:
    """One row of one tier as it moves through fusion, rerank and scoring."""

    id: UUID
    tier: str  # observations | summaries | facts | procedures
    bm25: float | None = None
    cos: float | None = None
    bm25_rank: int | None = None
    vec_rank: int | None = None
    rrf: float = 0.0
    rerank: float | None = None
    score: float = 0.0
    row: Any = None  # hydrated record
    embedding: list[float] | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    @property
    def base(self) -> float:
        return self.rerank if self.rerank is not None else self.rrf

    def log(self) -> dict[str, Any]:
        out: dict[str, Any] = {"id": str(self.id), "table": self.tier, "rrf": round(self.rrf, 6)}
        for key in ("bm25", "cos", "rerank"):
            value = getattr(self, key)
            if value is not None:
                out[key] = round(float(value), 6)
        out["score"] = round(self.score, 6)
        return out


def rrf_fuse(
    tier: str, keyword: Sequence[tuple[UUID, float]], vector: Sequence[tuple[UUID, float]], k: int = RRF_K
) -> dict[UUID, Candidate]:
    """Reciprocal-rank fusion of two ranked legs (each already ordered best first)."""
    out: dict[UUID, Candidate] = {}
    for rank, (cid, s) in enumerate(keyword, start=1):
        c = out.setdefault(cid, Candidate(cid, tier))
        c.bm25, c.bm25_rank = float(s), rank
        c.rrf += 1.0 / (k + rank)
    for rank, (cid, s) in enumerate(vector, start=1):
        c = out.setdefault(cid, Candidate(cid, tier))
        c.cos, c.vec_rank = float(s), rank
        c.rrf += 1.0 / (k + rank)
    return out


def recency(age_days: float, half_life: float | None) -> float:
    """``0.5 ** (age / half_life)``; 1.0 when the tier has no decay."""
    if half_life is None:
        return 1.0
    return float(0.5 ** (max(age_days, 0.0) / half_life))


def half_life_for(tier: str, obs_type: str | None = None) -> float | None:
    if tier == "observations":
        return HALF_LIFE_OBS_LONG if obs_type in LONG_LIVED_TYPES else HALF_LIFE_OBS_DEFAULT
    if tier == "summaries":
        return HALF_LIFE_SUMMARIES
    return None


def age_days(created_at: datetime | None, now: datetime) -> float:
    if created_at is None:
        return 0.0
    return max((now - created_at).total_seconds() / 86400.0, 0.0)


def final_score(
    base: float,
    *,
    importance: float = 0.5,
    confidence: float | None = None,
    recency_value: float | None = None,
    type_boost: float = 1.0,
    bonus: float = 0.0,
) -> float:
    """Spec 11.4: base x (0.5+0.5 imp) x (0.5+0.5 conf_or_1) x (0.3+0.7 rec_or_1) x type_boost (+ bonus)."""
    conf = 1.0 if confidence is None else confidence
    rec = 1.0 if recency_value is None else recency_value
    return base * (0.5 + 0.5 * importance) * (0.5 + 0.5 * conf) * (0.3 + 0.7 * rec) * type_boost + bonus


def sort_key(c: Candidate) -> tuple[float, str]:
    """Deterministic order: score desc, then id."""
    return (-c.score, str(c.id))


def mmr_order(cands: Sequence[Candidate], lam: float = MMR_LAMBDA) -> list[Candidate]:
    """Maximal marginal relevance over best-part embeddings (11.4), so paraphrases do not fill a slot.

    Relevance is the final score normalised to [0, 1] within the tier. Candidates without an
    embedding have zero similarity to everything.
    """
    ordered = sorted(cands, key=sort_key)
    if len(ordered) <= 1:
        return ordered
    top = max(c.score for c in ordered)
    rel = np.array([c.score / top if top > 0 else 0.0 for c in ordered])
    dims = next((len(c.embedding) for c in ordered if c.embedding), 0)
    if dims == 0:
        return ordered
    mat = np.zeros((len(ordered), dims))
    for i, c in enumerate(ordered):
        if c.embedding:
            v = np.asarray(c.embedding, dtype=float)
            n = np.linalg.norm(v)
            mat[i] = v / n if n > 0 else v
    sims = mat @ mat.T
    selected: list[int] = []
    remaining = list(range(len(ordered)))
    max_sim = np.zeros(len(ordered))
    while remaining:
        best = max(remaining, key=lambda i: (lam * rel[i] - (1 - lam) * max_sim[i], -i))
        selected.append(best)
        remaining.remove(best)
        max_sim = np.maximum(max_sim, sims[best])
    return [ordered[i] for i in selected]


T = TypeVar("T")


def budget_fill(items: Sequence[T], cost: Callable[[T], int], budget: int, max_items: int | None = None) -> list[T]:
    """Greedy in order: take an item when it still fits, skip it otherwise (11.5)."""
    out: list[T] = []
    used = 0
    for item in items:
        if max_items is not None and len(out) >= max_items:
            break
        c = cost(item)
        if used + c <= budget:
            out.append(item)
            used += c
    return out
