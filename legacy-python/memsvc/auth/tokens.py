"""Bearer tokens: ``mem_<prefix 8>_<secret 32>``, base62 (spec 5.2).

The server stores the prefix and ``HMAC-SHA256(TOKEN_PEPPER, secret)``; the raw token is shown once.
Lookup is by prefix, then a constant-time hash compare, then revocation/expiry/disabled-user checks.
"""

from __future__ import annotations

import hashlib
import hmac
import re
import secrets
import string
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from uuid import UUID

import asyncpg

from memsvc.auth.identity import Identity
from memsvc.auth.scopes import ALL_SCOPES, KIND_SCOPES, KIND_TTL
from memsvc.errors import ApiError
from memsvc.ids import uuid7

BASE62 = string.digits + string.ascii_uppercase + string.ascii_lowercase
TOKEN_RE = re.compile(r"^mem_([0-9A-Za-z]{8})_([0-9A-Za-z]{32})$")


def _rand62(n: int) -> str:
    return "".join(secrets.choice(BASE62) for _ in range(n))


def hash_secret(secret: str, pepper: str) -> bytes:
    return hmac.new(pepper.encode(), secret.encode(), hashlib.sha256).digest()


def parse_token(raw: str) -> tuple[str, str] | None:
    """Return ``(prefix, secret)`` or None when ``raw`` is not a well-formed token."""
    m = TOKEN_RE.match(raw.strip())
    return (m.group(1), m.group(2)) if m else None


@dataclass(frozen=True)
class MintedToken:
    token: str  # raw bearer value; show once, never store
    token_id: UUID
    prefix: str
    user_id: UUID
    client_id: UUID
    kind: str
    scopes: tuple[str, ...]
    expires_at: datetime | None


async def mint_token(
    conn: asyncpg.Connection,
    *,
    user_id: UUID,
    client_id: UUID,
    kind: str,
    pepper: str,
    scopes: Sequence[str] | None = None,
    expires_in: timedelta | None = None,
    never_expires: bool = False,
) -> MintedToken:
    """Create an api_tokens row. ``scopes`` defaults to the kind's scopes; expiry to the kind's TTL."""
    if kind not in KIND_SCOPES:
        raise ValueError(f"unknown token kind {kind!r}")
    chosen = tuple(scopes) if scopes is not None else KIND_SCOPES[kind]
    unknown = set(chosen) - ALL_SCOPES
    if unknown:
        raise ValueError(f"unknown scopes: {sorted(unknown)}")
    expires_at = None if never_expires else datetime.now(UTC) + (expires_in or KIND_TTL[kind])
    token_id = uuid7()
    for _ in range(5):  # prefix collisions are astronomically rare; retry anyway
        prefix, secret = _rand62(8), _rand62(32)
        try:
            await conn.execute(
                """INSERT INTO api_tokens (id, user_id, client_id, kind, prefix, secret_hash, scopes, expires_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8)""",
                token_id,
                user_id,
                client_id,
                kind,
                prefix,
                hash_secret(secret, pepper),
                list(chosen),
                expires_at,
            )
        except asyncpg.UniqueViolationError:
            continue
        return MintedToken(f"mem_{prefix}_{secret}", token_id, prefix, user_id, client_id, kind, chosen, expires_at)
    raise RuntimeError("could not allocate a unique token prefix")


async def revoke_token(conn: asyncpg.Connection, token_id: UUID) -> bool:
    """Revoke immediately (verification reads the row on every request). False if unknown/already revoked."""
    status = await conn.execute(
        "UPDATE api_tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL", token_id
    )
    return status.endswith(" 1")


def _unauthorized(code: str, message: str) -> ApiError:
    return ApiError(401, code, message, headers={"WWW-Authenticate": "Bearer"})


async def verify_token(conn: asyncpg.Connection, raw: str, pepper: str) -> Identity:
    """Resolve a raw bearer token to an :class:`Identity`, or raise ApiError(401)."""
    parsed = parse_token(raw)
    if parsed is None:
        raise _unauthorized("invalid_token", "malformed token")
    prefix, secret = parsed
    digest = hash_secret(secret, pepper)
    row = await conn.fetchrow(
        """SELECT t.id, t.user_id, t.client_id, t.kind, t.scopes, t.secret_hash, t.expires_at, t.revoked_at,
                  (t.last_used_at IS NULL OR t.last_used_at < now() - interval '1 minute') AS stale,
                  u.default_space_id, u.disabled_at, c.device_id, c.kind AS client_kind
             FROM api_tokens t
             JOIN users u ON u.id = t.user_id
             JOIN clients c ON c.id = t.client_id
            WHERE t.prefix = $1""",
        prefix,
    )
    if row is None or not hmac.compare_digest(bytes(row["secret_hash"]), digest):
        raise _unauthorized("invalid_token", "unknown token")
    if row["revoked_at"] is not None:
        raise _unauthorized("token_revoked", "token has been revoked")
    if row["expires_at"] is not None and row["expires_at"] <= datetime.now(UTC):
        raise _unauthorized("token_expired", "token has expired")
    if row["disabled_at"] is not None:
        raise _unauthorized("user_disabled", "user is disabled")
    if row["stale"]:  # last_used_at / last_seen_at at most once per minute
        await conn.execute(
            """WITH t AS (UPDATE api_tokens SET last_used_at = now() WHERE id = $1)
               UPDATE clients SET last_seen_at = now() WHERE id = $2""",
            row["id"],
            row["client_id"],
        )
    return Identity(
        user_id=row["user_id"],
        client_id=row["client_id"],
        scopes=frozenset(row["scopes"]),
        default_space_id=row["default_space_id"],
        token_id=row["id"],
        token_kind=row["kind"],
        device_id=row["device_id"],
        client_kind=row["client_kind"],
    )
