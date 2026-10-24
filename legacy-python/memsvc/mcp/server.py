"""MCP server (spec 7). Owner: memories.

``build_mcp_app(state)`` returns the ASGI app that app.py routes ``/mcp`` to. The request reaches
it with its path unchanged (``/mcp``), so an ``mcp`` SDK ``streamable_http_app()`` built with the
default ``streamable_http_path="/mcp"`` and ``stateless_http=True`` works as is. If the returned app
has a Starlette lifespan (the SDK's runs the session manager), app.py enters it during startup.

With mcp 2.x: ``mcp.server.mcpserver.MCPServer(...).streamable_http_app(stateless_http=True,
json_response=True, host="0.0.0.0")`` (``host`` other than localhost disables the SDK's DNS-rebinding
Host check, which would otherwise reject the tailnet hostname). Verified to work behind this route.

``state`` is the FastAPI ``app.state``: ``state.db`` (memsvc.db.Database), ``state.settings`` and
``state.tei`` are populated before the first request. Authenticate with
``memsvc.auth.authenticate_header(state.db, state.settings, headers["authorization"])`` and rate
limit with ``memsvc.auth.check_rate_limit``.
"""

from __future__ import annotations

from starlette.applications import Starlette
from starlette.datastructures import State
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route
from starlette.types import ASGIApp


def build_mcp_app(state: State) -> ASGIApp:
    async def not_implemented(request: Request) -> JSONResponse:
        return JSONResponse(
            {"error": {"code": "not_implemented", "message": "MCP server not built yet", "retryable": False}},
            status_code=501,
        )

    return Starlette(routes=[Route("/mcp", not_implemented, methods=["GET", "POST", "DELETE"])])
