from __future__ import annotations

import httpx


async def test_health_and_metrics(client: httpx.AsyncClient) -> None:
    r = await client.get("/health")
    assert r.status_code == 200
    body = r.json()
    assert body["status"] == "ok" and body["db"] is True and body["tei"] == {"embed": True, "rerank": True}
    r = await client.get("/metrics")
    assert r.status_code == 200 and "memsvc_tei_latency_seconds" in r.text


async def test_health_degraded_when_tei_down(app, client: httpx.AsyncClient) -> None:  # type: ignore[no-untyped-def]
    async def down() -> dict[str, bool]:
        return {"embed": False, "rerank": True}

    app.state.tei.health = down
    r = await client.get("/health")
    assert r.status_code == 200 and r.json()["status"] == "degraded"


async def test_error_shape(client: httpx.AsyncClient) -> None:
    r = await client.get("/v1/does-not-exist")
    assert r.status_code == 404
    assert r.json() == {"error": {"code": "not_found", "message": "Not Found", "retryable": False}}


async def test_mcp_route_mounted(client: httpx.AsyncClient) -> None:
    r = await client.post("/mcp", json={})
    assert r.status_code != 404


async def test_lifespan_runs_without_implemented_hooks(settings) -> None:  # type: ignore[no-untyped-def]
    from memsvc.app import create_app

    app = create_app(settings=settings)
    async with app.router.lifespan_context(app):
        assert app.state.db is not None
        async with app.state.db.app.acquire() as conn:
            assert await conn.fetchval("SELECT 1") == 1
    assert app.state.db is None
