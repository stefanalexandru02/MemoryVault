"""Spec 6.4: POST /v1/memories, DELETE /v1/memories/{id} (memories:write; ?hard=true needs admin).

Owner: memories. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
