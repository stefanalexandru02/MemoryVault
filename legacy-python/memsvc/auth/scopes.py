"""Token scopes and kinds (spec 5.2)."""

from __future__ import annotations

from datetime import timedelta
from typing import Literal

EVENTS_WRITE = "events:write"  # POST /v1/sessions, /v1/sessions/{id}/events, /v1/sessions/{id}/end
MEMORIES_READ = "memories:read"  # recall, context, search, timeline, get, explain, work-state read, MCP read tools
MEMORIES_WRITE = "memories:write"  # memory_remember, memory_forget (soft), work_state_write, feedback
WORK_LEASE = "work:lease"  # POST /v1/work/lease, /v1/work/{id}/result|fail|heartbeat
ADMIN = "admin"  # token mint/revoke, users/spaces, hard delete, consolidation trigger, revert

ALL_SCOPES = frozenset({EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE, ADMIN})

TokenKind = Literal["device", "runner", "admin"]

KIND_SCOPES: dict[str, tuple[str, ...]] = {
    "device": (EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE),
    "runner": (WORK_LEASE, MEMORIES_READ),
    "admin": (ADMIN, EVENTS_WRITE, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE),
}

KIND_TTL: dict[str, timedelta] = {
    "device": timedelta(days=365),
    "runner": timedelta(days=90),
    "admin": timedelta(days=30),
}

# Default clients.kind for a client created alongside a token of this kind.
KIND_CLIENT_KIND: dict[str, str] = {"device": "claude-code", "runner": "runner", "admin": "cli"}
