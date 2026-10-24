"""Async client for Text Embeddings Inference (spec 13), plus a deterministic fake for tests.

``get_tei()`` returns the process-wide client: :class:`FakeTEI` when ``FAKE_TEI=1``, else
:class:`TEIClient`. Tests and the app may install one with ``set_tei()``.

Metrics ``memsvc_tei_latency_seconds{service}`` and ``memsvc_tei_errors_total{service}`` are defined
here; ``memsvc.metrics`` must not redefine them.
"""

from __future__ import annotations

import hashlib
import math
import re
import time
from typing import Protocol

import httpx
from prometheus_client import Counter, Histogram

from memsvc.config import Settings, get_settings

TEI_LATENCY = Histogram(
    "memsvc_tei_latency_seconds",
    "TEI call latency",
    ["service"],
    buckets=(0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.25, 0.5, 1, 2, 5),
)
TEI_ERRORS = Counter("memsvc_tei_errors_total", "TEI call errors", ["service"])


class TEIError(Exception):
    """TEI unreachable or returned an error."""


class TEITimeout(TEIError):
    """TEI did not answer within the configured timeout (rerank falls back to RRF, spec 11.3)."""


class Embedder(Protocol):
    model: str
    dims: int

    async def embed(self, texts: list[str]) -> list[list[float]]: ...

    async def rerank(self, query: str, texts: list[str]) -> list[float]: ...

    async def health(self) -> dict[str, bool]: ...

    async def aclose(self) -> None: ...


class TEIClient:
    """``POST {embed_url}/embed`` and ``POST {rerank_url}/rerank`` (HuggingFace TEI HTTP API)."""

    def __init__(
        self,
        embed_url: str,
        rerank_url: str,
        *,
        model: str,
        dims: int,
        embed_timeout_s: float = 2.0,
        rerank_timeout_s: float = 0.25,
        client: httpx.AsyncClient | None = None,
    ) -> None:
        self.embed_url = embed_url.rstrip("/")
        self.rerank_url = rerank_url.rstrip("/")
        self.model = model
        self.dims = dims
        self.embed_timeout_s = embed_timeout_s
        self.rerank_timeout_s = rerank_timeout_s
        self._client = client or httpx.AsyncClient()

    async def _post(self, service: str, url: str, body: dict, timeout_s: float) -> object:
        start = time.perf_counter()
        try:
            resp = await self._client.post(url, json=body, timeout=timeout_s)
            resp.raise_for_status()
            return resp.json()
        except httpx.TimeoutException as e:
            TEI_ERRORS.labels(service).inc()
            raise TEITimeout(f"{service} timed out after {timeout_s}s") from e
        except (httpx.HTTPError, ValueError) as e:
            TEI_ERRORS.labels(service).inc()
            raise TEIError(f"{service} failed: {e}") from e
        finally:
            TEI_LATENCY.labels(service).observe(time.perf_counter() - start)

    async def embed(self, texts: list[str]) -> list[list[float]]:
        """Dense, L2-normalised vectors, one per text, in input order. Inputs are truncated by TEI."""
        if not texts:
            return []
        data = await self._post(
            "embed",
            f"{self.embed_url}/embed",
            {"inputs": texts, "truncate": True, "normalize": True},
            self.embed_timeout_s,
        )
        if not isinstance(data, list) or len(data) != len(texts):
            raise TEIError("embed returned an unexpected shape")
        return [[float(x) for x in vec] for vec in data]

    async def rerank(self, query: str, texts: list[str]) -> list[float]:
        """Relevance score per text, aligned with ``texts`` (TEI returns them sorted by score)."""
        if not texts:
            return []
        data = await self._post(
            "rerank",
            f"{self.rerank_url}/rerank",
            {"query": query, "texts": texts, "truncate": True, "raw_scores": False},
            self.rerank_timeout_s,
        )
        if not isinstance(data, list):
            raise TEIError("rerank returned an unexpected shape")
        scores = [0.0] * len(texts)
        for item in data:
            scores[int(item["index"])] = float(item["score"])
        return scores

    async def health(self) -> dict[str, bool]:
        out: dict[str, bool] = {}
        for name, url in (("embed", self.embed_url), ("rerank", self.rerank_url)):
            try:
                r = await self._client.get(f"{url}/health", timeout=1.0)
                out[name] = r.status_code == 200
            except httpx.HTTPError:
                out[name] = False
        return out

    async def aclose(self) -> None:
        await self._client.aclose()


_WORD_RE = re.compile(r"[\w]+", re.UNICODE)


def _tokens(text: str) -> list[str]:
    return [t.lower() for t in _WORD_RE.findall(text)]


class FakeTEI:
    """Deterministic in-process stand-in (``FAKE_TEI=1``).

    ``embed``: hashed bag of words (blake2b per token -> dimension and sign), L2-normalised, so texts
    sharing words have higher cosine similarity. ``rerank``: fraction of query tokens present in the text.
    """

    def __init__(self, *, model: str = "bge-m3", dims: int = 1024) -> None:
        self.model = model
        self.dims = dims
        self.calls: list[tuple[str, int]] = []  # (method, n_texts) for test assertions

    def _vector(self, text: str) -> list[float]:
        vec = [0.0] * self.dims
        toks = _tokens(text) or ["\x00empty"]
        for tok in toks:
            h = hashlib.blake2b(tok.encode(), digest_size=8).digest()
            idx = int.from_bytes(h[:4]) % self.dims
            vec[idx] += 1.0 if h[4] & 1 else -1.0
        norm = math.sqrt(sum(x * x for x in vec)) or 1.0
        return [x / norm for x in vec]

    async def embed(self, texts: list[str]) -> list[list[float]]:
        self.calls.append(("embed", len(texts)))
        return [self._vector(t) for t in texts]

    async def rerank(self, query: str, texts: list[str]) -> list[float]:
        self.calls.append(("rerank", len(texts)))
        q = set(_tokens(query))
        if not q:
            return [0.0] * len(texts)
        return [len(q & set(_tokens(t))) / len(q) for t in texts]

    async def health(self) -> dict[str, bool]:
        return {"embed": True, "rerank": True}

    async def aclose(self) -> None:
        return None


def build_tei(settings: Settings) -> Embedder:
    if settings.fake_tei:
        return FakeTEI(model=settings.embed_model, dims=settings.embed_dims)
    return TEIClient(
        settings.tei_embed_url,
        settings.tei_rerank_url,
        model=settings.embed_model,
        dims=settings.embed_dims,
        embed_timeout_s=settings.tei_embed_timeout_s,
        rerank_timeout_s=settings.tei_rerank_timeout_s,
    )


_tei: Embedder | None = None


def get_tei() -> Embedder:
    """The process-wide embedder (built from settings on first use)."""
    global _tei
    if _tei is None:
        _tei = build_tei(get_settings())
    return _tei


def set_tei(client: Embedder | None) -> None:
    global _tei
    _tei = client
