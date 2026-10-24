"""Spec 6.6, 9.3, 9.4 (work:lease): POST /v1/work/lease, /v1/work/{id}/result, /v1/work/{id}/fail,
/v1/work/{id}/heartbeat. Owner: ingest.

All four run as the caller (``user_tx``): only packets in spaces the caller can write are visible.
"""

from __future__ import annotations

from uuid import UUID

from fastapi import APIRouter, Depends, Request

from memsvc.auth import WORK_LEASE, Identity, require_scope
from memsvc.db import user_tx
from memsvc.errors import ApiError
from memsvc.packets import appliers, queue, windows
from memsvc.schemas.outputs import OutputInvalid, validate_output
from memsvc.schemas.work import (
    FailRequest,
    FailResponse,
    HeartbeatResponse,
    LeaseRequest,
    LeaseResponse,
    PacketRecord,
    ResultRequest,
    ResultResponse,
)

router = APIRouter()


@router.post("/v1/work/lease", response_model=LeaseResponse)
async def lease(
    body: LeaseRequest, request: Request, identity: Identity = Depends(require_scope(WORK_LEASE))
) -> LeaseResponse:
    settings = request.app.state.settings
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        packets = await queue.lease(
            conn,
            client_id=identity.client_id,
            kinds=list(body.kinds),
            max_packets=body.max,
            lease_seconds=body.lease_seconds,
            preferred_runner_seconds=settings.preferred_runner_seconds,
            session_id=body.session_id,
        )
    return LeaseResponse(packets=packets)


@router.post("/v1/work/{packet_id}/result", response_model=ResultResponse)
async def post_result(
    packet_id: UUID, body: ResultRequest, request: Request, identity: Identity = Depends(require_scope(WORK_LEASE))
) -> ResultResponse:
    """Validate (fence stripping only, 9.4) and apply in one transaction. Idempotent on the packet: a repeat
    post returns the first application's counts. Invalid output -> 422 ``schema_invalid`` with ``retry_with``;
    the lease is kept so the runner can retry."""
    invalid: str | None = None
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        row = await queue.lock_packet(conn, packet_id)
        if row["status"] == "done":
            return ResultResponse(applied=row["applied"] or {}, duplicate=True)
        queue.require_lease(row, identity.client_id)
        try:
            output = validate_output(row["output_schema"], body.output)
        except OutputInvalid as e:
            invalid = str(e)
            await conn.execute(
                "UPDATE work_packets SET last_error = $2 WHERE id = $1", packet_id, f"schema_invalid: {e}"
            )
        else:
            meta = body.meta.model_dump(mode="json", exclude_none=True)
            packet = PacketRecord.model_validate({**dict(row), "result_meta": meta})
            counts = await appliers.apply(conn, packet, output)
            await conn.execute(
                """UPDATE work_packets SET status = 'done', result = $2, result_meta = $3, applied = $4,
                          done_at = now(), lease_until = NULL, last_error = NULL
                    WHERE id = $1""",
                packet_id,
                output.model_dump(mode="json"),
                meta,
                counts,
            )
            await windows.after_packet_settled(conn, packet.kind, packet.session_id, packet.inputs)
    if invalid is not None:
        raise ApiError(
            422, "schema_invalid", "output does not match the packet's output_schema", extra={"retry_with": invalid}
        )
    return ResultResponse(applied=counts)


@router.post("/v1/work/{packet_id}/fail", response_model=FailResponse)
async def post_fail(
    packet_id: UUID, body: FailRequest, request: Request, identity: Identity = Depends(require_scope(WORK_LEASE))
) -> FailResponse:
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        row = await queue.fail(conn, packet_id, identity.client_id, body.error, body.detail)
    return FailResponse(
        id=row["id"],
        status=row["status"],
        attempts=row["attempts"],
        available_after=row["available_after"] if row["status"] == "pending" else None,
    )


@router.post("/v1/work/{packet_id}/heartbeat", response_model=HeartbeatResponse)
async def post_heartbeat(
    packet_id: UUID, request: Request, identity: Identity = Depends(require_scope(WORK_LEASE))
) -> HeartbeatResponse:
    settings = request.app.state.settings
    async with user_tx(request.app.state.db.app, identity.user_id, identity.client_id) as conn:
        row = await queue.heartbeat(conn, packet_id, identity.client_id, settings.lease_seconds_default)
    return HeartbeatResponse(id=row["id"], lease_until=row["lease_until"])
