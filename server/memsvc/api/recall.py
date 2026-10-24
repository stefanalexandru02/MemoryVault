"""Spec 6.3: POST /v1/recall, GET /v1/context, POST /v1/recall/{retrieval_id}/feedback.

Spec 6.4: GET /v1/search, GET /v1/timeline, POST /v1/memories/get, GET /v1/memories/{id}/explain,
GET /v1/observations/by-file.

Owner: recall. Routes are declared with full paths (no router prefix), e.g. ``@router.post("/v1/...")``.
Auth: ``Depends(memsvc.auth.require_scope(...))``; DB: ``request.app.state.db`` with ``memsvc.db.user_tx``.
"""

from fastapi import APIRouter

router = APIRouter()
