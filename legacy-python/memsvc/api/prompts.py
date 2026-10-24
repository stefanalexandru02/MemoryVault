"""Spec 10.1: GET /v1/prompts/{name} (memories:read). Owner: ingest.

``name`` may be ``observer.code`` (newest loaded version) or a full ref ``observer.code@v3``; the
``version`` query parameter also selects one.
"""

from __future__ import annotations

from fastapi import APIRouter, Depends

from memsvc.auth import MEMORIES_READ, Identity, require_scope
from memsvc.errors import ApiError
from memsvc.prompts import registry
from memsvc.schemas.prompts import PromptOut

router = APIRouter()


@router.get("/v1/prompts/{name}", response_model=PromptOut)
async def get_prompt(
    name: str, version: str | None = None, identity: Identity = Depends(require_scope(MEMORIES_READ))
) -> PromptOut:
    try:
        spec = registry.get(name, version)
    except KeyError as e:
        raise ApiError(404, "not_found", str(e.args[0]) if e.args else "unknown prompt") from e
    return PromptOut(
        name=spec.name,
        version=spec.version,
        prompt_ref=spec.prompt_ref,
        system=spec.system,
        user_template=spec.user_template,
        output_schema=spec.output_schema,
        model_hint=spec.model_hint,
        versions=registry.versions(spec.name),
    )
