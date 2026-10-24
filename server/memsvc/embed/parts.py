"""Embedding parts per row (spec 13). Owner: worker."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any


def parts_for(table: str, row: Mapping[str, Any]) -> list[tuple[str, str]]:
    """``[(part, text), ...]`` to embed for a row of ``table``.

    observations: ``title``, ``narrative``, ``fact:<i>``; summaries: ``request``, ``learned``,
    ``next_steps``; facts: ``value`` (text = value_text); procedures: ``trigger``, ``body``;
    entities: ``name``. Empty texts are skipped; texts are truncated to ~512 tokens.
    """
    raise NotImplementedError
