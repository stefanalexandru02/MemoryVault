"""Spec 6.6, 9.3, 9.4 (work:lease): POST /v1/work/lease, /v1/work/{id}/result, /v1/work/{id}/fail,
/v1/work/{id}/heartbeat.

Owner: ingest. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
