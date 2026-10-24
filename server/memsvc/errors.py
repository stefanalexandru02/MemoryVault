"""API error type. Every error response body is
``{"error": {"code": str, "message": str, "retryable": bool, ...extra}}`` (spec 6)."""

from __future__ import annotations

from typing import Any


class ApiError(Exception):
    """Raise from any handler or dependency; app.py renders it in the spec error shape.

    ``extra`` keys are merged into the error object (e.g. ``retry_with`` for schema_invalid).
    ``headers`` are added to the response (e.g. ``Retry-After``).
    """

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        *,
        retryable: bool = False,
        extra: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.retryable = retryable
        self.extra = extra or {}
        self.headers = headers or {}

    def body(self) -> dict[str, Any]:
        return {"error": {"code": self.code, "message": self.message, "retryable": self.retryable, **self.extra}}


def not_found(what: str = "not found") -> ApiError:
    return ApiError(404, "not_found", what)


def forbidden(message: str = "forbidden") -> ApiError:
    return ApiError(403, "forbidden", message)


def bad_request(message: str, code: str = "invalid_request") -> ApiError:
    return ApiError(400, code, message)
