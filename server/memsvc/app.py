"""FastAPI application factory.

``create_app()`` wires every module's router, the MCP app at ``/mcp``, ``/health`` and ``/metrics``,
and the spec error shape for every error. Tests pass ``settings``/``db``/``tei`` directly (httpx's
ASGITransport does not run lifespan events); ``memsvc serve`` lets the lifespan open the pools.
"""

from __future__ import annotations

import asyncio
import logging
from collections.abc import AsyncIterator, Callable
from contextlib import AsyncExitStack, asynccontextmanager, suppress
from typing import Any

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, Response
from prometheus_client import CONTENT_TYPE_LATEST, generate_latest
from starlette.exceptions import HTTPException as StarletteHTTPException
from starlette.routing import Route

from memsvc import __version__
from memsvc.api import admin, events, memories, prompts, recall, sessions, work, work_state
from memsvc.config import Settings, get_settings
from memsvc.db import Database, caps, connect
from memsvc.embed.tei import Embedder, build_tei, set_tei
from memsvc.errors import ApiError
from memsvc.mcp.server import build_mcp_app

log = logging.getLogger(__name__)

ROUTERS = (sessions, events, work, prompts, recall, memories, work_state, admin)


def _error(status: int, code: str, message: str, retryable: bool = False, headers: dict | None = None) -> JSONResponse:
    return JSONResponse(
        {"error": {"code": code, "message": message, "retryable": retryable}}, status_code=status, headers=headers
    )


async def _api_error(request: Request, exc: Exception) -> JSONResponse:
    assert isinstance(exc, ApiError)
    return JSONResponse(exc.body(), status_code=exc.status, headers=exc.headers or None)


async def _http_error(request: Request, exc: Exception) -> JSONResponse:
    assert isinstance(exc, StarletteHTTPException)
    code = {401: "unauthorized", 403: "forbidden", 404: "not_found", 405: "method_not_allowed"}.get(
        exc.status_code, "http_error"
    )
    return _error(exc.status_code, code, str(exc.detail), exc.status_code >= 500, getattr(exc, "headers", None))


async def _validation_error(request: Request, exc: Exception) -> JSONResponse:
    assert isinstance(exc, RequestValidationError)
    return JSONResponse(
        {
            "error": {
                "code": "invalid_request",
                "message": "request validation failed",
                "retryable": False,
                "details": exc.errors(),
            }
        },
        status_code=422,
    )


async def _unhandled(request: Request, exc: Exception) -> JSONResponse:
    log.exception("unhandled error on %s %s", request.method, request.url.path)
    return _error(500, "internal", "internal server error", True)


def _hook(name: str, fn: Callable[..., Any], *args: Any) -> Any:
    """Call an optional startup hook owned by another module; tolerate it not being implemented yet."""
    try:
        return fn(*args)
    except NotImplementedError:
        log.warning("startup hook %s not implemented yet; skipping", name)
        return None


def create_app(settings: Settings | None = None, db: Database | None = None, tei: Embedder | None = None) -> FastAPI:
    settings = settings or get_settings()
    tei = tei or build_tei(settings)
    set_tei(tei)  # recall/worker code reads the embedder via memsvc.embed.tei.get_tei()

    mcp_app = None  # built below; the lifespan needs a reference

    @asynccontextmanager
    async def lifespan(app: FastAPI) -> AsyncIterator[None]:
        from memsvc.prompts import registry
        from memsvc.recall import cache

        owns_db = app.state.db is None
        if owns_db:
            app.state.db = await connect(settings)
        _hook("prompts.registry.load", registry.load, settings.prompts_dir)
        listener = None
        try:
            listener = await cache.start_listener(app.state.db)
        except NotImplementedError:
            log.warning("startup hook recall.cache.start_listener not implemented yet; skipping")
        async with AsyncExitStack() as stack:
            lifespan_context = getattr(getattr(mcp_app, "router", None), "lifespan_context", None)
            if lifespan_context is not None:
                await stack.enter_async_context(lifespan_context(mcp_app))
            try:
                yield
            finally:
                if listener is not None:
                    listener.cancel()
                    with suppress(asyncio.CancelledError):
                        await listener
                if owns_db:
                    await app.state.db.close()
                    app.state.db = None
                await tei.aclose()

    app = FastAPI(title="MemoryVault", version=__version__, lifespan=lifespan)
    app.state.settings = settings
    app.state.db = db
    app.state.tei = tei

    app.add_exception_handler(ApiError, _api_error)
    app.add_exception_handler(StarletteHTTPException, _http_error)
    app.add_exception_handler(RequestValidationError, _validation_error)
    app.add_exception_handler(Exception, _unhandled)

    for module in ROUTERS:
        app.include_router(module.router)

    mcp_app = build_mcp_app(app.state)
    app.router.routes.append(Route("/mcp", endpoint=mcp_app))

    @app.get("/health", include_in_schema=False)
    async def health(request: Request) -> JSONResponse:
        state_db: Database | None = request.app.state.db
        db_ok, pg_search = False, None
        if state_db is not None:
            try:
                async with state_db.app.acquire() as conn:
                    db_ok = await asyncio.wait_for(conn.fetchval("SELECT 1"), timeout=2.0) == 1
                    pg_search = caps(conn).pg_search
            except Exception:  # noqa: BLE001
                log.warning("health: database check failed", exc_info=True)
        try:
            tei_status = await request.app.state.tei.health()
        except Exception:  # noqa: BLE001
            tei_status = {"embed": False, "rerank": False}
        status = "down" if not db_ok else ("ok" if all(tei_status.values()) else "degraded")
        body = {"status": status, "db": db_ok, "tei": tei_status, "version": __version__, "pg_search": pg_search}
        return JSONResponse(body, status_code=200 if db_ok else 503)

    @app.get("/metrics", include_in_schema=False)
    async def metrics() -> Response:
        return Response(generate_latest(), media_type=CONTENT_TYPE_LATEST)

    return app
