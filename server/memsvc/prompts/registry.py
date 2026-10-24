"""Prompt registry: loads ``server/prompts/*.yaml`` at start (spec 10.1). Owner: ingest.

Each YAML file has ``name``, ``version``, ``system``, ``user_template`` (Jinja2), ``output_schema``,
``model_hint`` (see ``memsvc.schemas.prompts.PromptSpec``). ``prompt_ref = name@version``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from memsvc.schemas.prompts import PromptSpec, RenderedPrompt


def load(prompts_dir: Path) -> None:
    """(Re)load every prompt file in ``prompts_dir``. Called at app/worker startup."""
    raise NotImplementedError


def get(name: str, version: str | None = None) -> PromptSpec:
    """Prompt by name and version; ``version=None`` = highest loaded version. ``name`` may also be a
    full ``name@version`` ref. Raises KeyError when unknown."""
    raise NotImplementedError


def render(prompt: PromptSpec, inputs: dict[str, Any]) -> RenderedPrompt:
    """Render ``user_template`` with the packet ``inputs`` (StrictUndefined); ``system`` is returned as is."""
    raise NotImplementedError
