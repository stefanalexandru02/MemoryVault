from __future__ import annotations

import math

import httpx
import pytest
import respx

from memsvc.embed.tei import FakeTEI, TEIClient, TEIError, TEITimeout


def _cos(a: list[float], b: list[float]) -> float:
    return sum(x * y for x, y in zip(a, b, strict=True))


async def test_fake_tei_deterministic_and_lexical() -> None:
    tei = FakeTEI(dims=1024)
    a, b, c = await tei.embed(["deploy the memory api", "redeploy memory api after migration", "banana bread"])
    assert len(a) == 1024 and math.isclose(math.sqrt(sum(x * x for x in a)), 1.0, rel_tol=1e-9)
    assert (await FakeTEI(dims=1024).embed(["deploy the memory api"]))[0] == a
    assert _cos(a, b) > _cos(a, c)
    scores = await tei.rerank("memory api", ["the memory api", "api", "nothing"])
    assert scores == [1.0, 0.5, 0.0]
    assert await tei.health() == {"embed": True, "rerank": True}


@respx.mock
async def test_tei_client_embed_and_rerank_order() -> None:
    respx.post("http://e/embed").mock(return_value=httpx.Response(200, json=[[0.1, 0.2], [0.3, 0.4]]))
    respx.post("http://r/rerank").mock(
        return_value=httpx.Response(200, json=[{"index": 1, "score": 0.9}, {"index": 0, "score": 0.1}])
    )
    tei = TEIClient("http://e", "http://r", model="m", dims=2)
    assert await tei.embed(["a", "b"]) == [[0.1, 0.2], [0.3, 0.4]]
    assert await tei.rerank("q", ["x", "y"]) == [0.1, 0.9]
    assert await tei.embed([]) == []
    await tei.aclose()


@respx.mock
async def test_tei_client_errors() -> None:
    respx.post("http://r/rerank").mock(side_effect=httpx.ReadTimeout("slow"))
    respx.post("http://e/embed").mock(return_value=httpx.Response(503))
    respx.get("http://e/health").mock(return_value=httpx.Response(200))
    respx.get("http://r/health").mock(side_effect=httpx.ConnectError("down"))
    tei = TEIClient("http://e", "http://r", model="m", dims=2)
    with pytest.raises(TEITimeout):
        await tei.rerank("q", ["x"])
    with pytest.raises(TEIError):
        await tei.embed(["a"])
    assert await tei.health() == {"embed": True, "rerank": False}
    await tei.aclose()
