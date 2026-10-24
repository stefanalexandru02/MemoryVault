"""Spec 6.2, 9.1: POST /v1/sessions/{id}/events (events:write): clamp, window assignment, extract packets.

Owner: ingest. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
