"""Spec 6.7: /v1/admin/* (admin scope): users, spaces, memberships, tokens, consolidate, revert, jobs, packets, reindex.

Owner: consolidation. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
