"""Spec 6.5: GET /v1/work-state (memories:read), POST /v1/work-state (memories:write).

Owner: memories. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
