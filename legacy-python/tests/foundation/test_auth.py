from __future__ import annotations

from datetime import timedelta

import pytest
from fastapi import Depends

from memsvc.app import create_app
from memsvc.auth import (
    ADMIN,
    EVENTS_WRITE,
    MEMORIES_READ,
    Identity,
    mint_token,
    require_scope,
    resolve_read_spaces,
    resolve_write_space,
    revoke_token,
    verify_token,
)
from memsvc.auth.admin import add_membership, create_space
from memsvc.auth.deps import check_rate_limit
from memsvc.auth.tokens import TOKEN_RE, hash_secret, parse_token
from memsvc.config import Settings
from memsvc.db import Database, system_tx, user_tx
from memsvc.errors import ApiError


async def _verify(db: Database, settings: Settings, raw: str) -> Identity:
    async with db.app.acquire() as conn:
        return await verify_token(conn, raw, settings.token_pepper)


async def test_mint_and_verify(db: Database, settings: Settings, make_identity) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity()
    assert TOKEN_RE.match(seeded.token)
    ident = await _verify(db, settings, seeded.token)
    assert ident.user_id == seeded.user_id and ident.client_id == seeded.client_id
    assert ident.default_space_id == seeded.space_id
    assert ident.scopes == {"events:write", "memories:read", "memories:write", "work:lease"}
    assert ident.device_id == seeded.device_id and ident.token_kind == "device"
    async with db.app.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT prefix, secret_hash, last_used_at, expires_at FROM api_tokens WHERE id=$1", seeded.token_id
        )
    prefix, secret = parse_token(seeded.token)  # type: ignore[misc]
    assert row["prefix"] == prefix
    assert bytes(row["secret_hash"]) == hash_secret(secret, settings.token_pepper)
    assert secret not in str(row)  # raw secret never stored
    assert row["last_used_at"] is not None
    assert row["expires_at"] is not None


@pytest.mark.parametrize(
    "kind,scopes", [("runner", {"work:lease", "memories:read"}), ("admin", {"admin", "events:write", "memories:read",
                                                                           "memories:write", "work:lease"})]
)  # fmt: skip
async def test_kind_scopes(db: Database, settings: Settings, make_identity, kind: str, scopes: set[str]) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity(kind=kind)
    assert seeded.identity.scopes == scopes


@pytest.mark.parametrize(
    "raw", ["", "mem_short_x", "Bearer abc", "mem_ABCDEFGH_" + "x" * 31, "mem_ABCDEFG!_" + "x" * 32]
)
async def test_malformed(db: Database, settings: Settings, raw: str) -> None:
    with pytest.raises(ApiError) as e:
        await _verify(db, settings, raw)
    assert e.value.status == 401 and e.value.code == "invalid_token"


async def test_wrong_secret_and_wrong_pepper(db: Database, settings: Settings, make_identity) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity()
    tampered = seeded.token[:-1] + ("A" if seeded.token[-1] != "A" else "B")
    with pytest.raises(ApiError) as e:
        await _verify(db, settings, tampered)
    assert e.value.code == "invalid_token"
    async with db.app.acquire() as conn:
        with pytest.raises(ApiError):
            await verify_token(conn, seeded.token, "other-pepper")


async def test_revoke(db: Database, settings: Settings, make_identity) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity()
    async with system_tx(db.system) as conn:
        assert await revoke_token(conn, seeded.token_id)
        assert not await revoke_token(conn, seeded.token_id)
    with pytest.raises(ApiError) as e:
        await _verify(db, settings, seeded.token)
    assert e.value.code == "token_revoked"


async def test_expiry(db: Database, settings: Settings, make_identity) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity()
    async with system_tx(db.system) as conn:
        expired = await mint_token(
            conn, user_id=seeded.user_id, client_id=seeded.client_id, kind="device", pepper=settings.token_pepper,
            expires_in=timedelta(seconds=-1),
        )  # fmt: skip
    with pytest.raises(ApiError) as e:
        await _verify(db, settings, expired.token)
    assert e.value.code == "token_expired"


async def test_disabled_user(db: Database, settings: Settings, make_identity) -> None:  # type: ignore[no-untyped-def]
    seeded = await make_identity()
    async with system_tx(db.system) as conn:
        await conn.execute("UPDATE users SET disabled_at = now() WHERE id = $1", seeded.user_id)
    with pytest.raises(ApiError) as e:
        await _verify(db, settings, seeded.token)
    assert e.value.code == "user_disabled"


def _scoped_app(settings: Settings, db: Database, fake_tei):  # type: ignore[no-untyped-def]
    app = create_app(settings=settings, db=db, tei=fake_tei)

    @app.get("/_t/read")
    async def read(identity: Identity = Depends(require_scope(MEMORIES_READ))) -> dict:
        return {"user_id": str(identity.user_id)}

    @app.post("/_t/events")
    async def events(identity: Identity = Depends(require_scope(EVENTS_WRITE))) -> dict:
        return {"ok": True}

    @app.get("/_t/admin")
    async def admin(identity: Identity = Depends(require_scope(ADMIN))) -> dict:
        return {"ok": True}

    return app


async def _call(app, method: str, path: str, token: str | None = None):  # type: ignore[no-untyped-def]
    import httpx

    headers = {"Authorization": f"Bearer {token}"} if token else {}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as c:
        return await c.request(method, path, headers=headers)


async def test_scope_enforcement(db: Database, settings: Settings, fake_tei, make_identity) -> None:  # type: ignore[no-untyped-def]
    app = _scoped_app(settings, db, fake_tei)
    device, runner, admin = await make_identity(), await make_identity(kind="runner"), await make_identity(kind="admin")

    r = await _call(app, "GET", "/_t/read")
    assert r.status_code == 401
    assert r.json()["error"]["code"] == "unauthorized" and r.headers["www-authenticate"] == "Bearer"
    r = await _call(app, "GET", "/_t/read", "mem_nottoken")
    assert r.status_code == 401 and r.json()["error"] == {
        "code": "invalid_token", "message": "malformed token", "retryable": False,
    }  # fmt: skip

    assert (await _call(app, "GET", "/_t/read", device.token)).json() == {"user_id": str(device.user_id)}
    assert (await _call(app, "POST", "/_t/events", device.token)).status_code == 200
    r = await _call(app, "GET", "/_t/admin", device.token)
    assert r.status_code == 403 and r.json()["error"]["code"] == "insufficient_scope"

    assert (await _call(app, "GET", "/_t/read", runner.token)).status_code == 200
    assert (await _call(app, "POST", "/_t/events", runner.token)).status_code == 403
    assert (await _call(app, "GET", "/_t/admin", runner.token)).status_code == 403

    for path, method in (("/_t/read", "GET"), ("/_t/events", "POST"), ("/_t/admin", "GET")):
        assert (await _call(app, method, path, admin.token)).status_code == 200


async def test_rate_limit(db: Database, settings: Settings, fake_tei, make_identity) -> None:  # type: ignore[no-untyped-def]
    limited = settings.model_copy(update={"rate_limit_default_per_min": 3, "rate_limit_events_per_min": 5})
    app = _scoped_app(limited, db, fake_tei)
    seeded = await make_identity()
    codes = [(await _call(app, "GET", "/_t/read", seeded.token)).status_code for _ in range(3)]
    assert codes == [200, 200, 200]
    r = await _call(app, "GET", "/_t/read", seeded.token)
    assert r.status_code == 429
    assert r.json()["error"]["code"] == "rate_limited" and r.json()["error"]["retryable"] is True
    assert 1 <= int(r.headers["retry-after"]) <= 60
    # separate bucket for events:write
    assert (await _call(app, "POST", "/_t/events", seeded.token)).status_code == 200


async def test_rate_limit_fails_open() -> None:
    class Broken:
        async def fetchrow(self, *a: object) -> None:
            raise ConnectionError("db down")

    await check_rate_limit(Broken(), "token:x:default", 1)  # type: ignore[arg-type]


async def test_space_resolution(db: Database, make_identity) -> None:  # type: ignore[no-untyped-def]
    alice, bob = await make_identity(), await make_identity()
    async with system_tx(db.system) as conn:
        shared = await create_space(conn, name="family", kind="shared", owner_id=alice.user_id)
        await add_membership(conn, space_id=shared, user_id=bob.user_id, role="reader")
        other_shared = await create_space(conn, name="other", kind="shared", owner_id=alice.user_id)

    async with user_tx(db.app, bob.user_id, bob.client_id) as conn:
        assert await resolve_read_spaces(conn, bob.identity) == [bob.space_id, shared]
        assert await resolve_read_spaces(conn, bob.identity, [shared]) == [shared]
        with pytest.raises(ApiError) as e:
            await resolve_read_spaces(conn, bob.identity, [alice.space_id])
        assert e.value.status == 403
        with pytest.raises(ApiError):
            await resolve_read_spaces(conn, bob.identity, [other_shared])

        assert await resolve_write_space(conn, bob.identity) == bob.space_id
        with pytest.raises(ApiError) as e:
            await resolve_write_space(conn, bob.identity, shared)  # reader only
        assert e.value.status == 403
    async with user_tx(db.app, alice.user_id, alice.client_id) as conn:
        assert await resolve_write_space(conn, alice.identity, shared) == shared
