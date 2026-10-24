"""Nightly planner (spec 12.1), run by ``memsvc planner``. Owner: consolidation."""

from __future__ import annotations

from uuid import UUID

from memsvc.db import Database
from memsvc.schemas.admin import ConsolidateResponse


async def run_planner(db: Database, space_id: UUID | None = None) -> ConsolidateResponse:
    """Plan one consolidation run for ``space_id`` (None = every space) using ``system_tx``:
    fact clusters, decay/archival, day/week summaries, procedures, profiles, denormalisation."""
    raise NotImplementedError
