"""Prompt registry: loads ``server/prompts/*.yaml`` at start (spec 10.1). Owner: ingest.

Each YAML file holds one prompt with ``name``, ``version``, ``system``, ``user_template`` (Jinja2),
``output_schema``, ``model_hint`` (see ``memsvc.schemas.prompts.PromptSpec``). ``prompt_ref = name@version``.

Templates render with Jinja2 defaults (no autoescape, no block trimming) and ``StrictUndefined``, so a
template naming a field the packet inputs lack fails loudly. ``tojson`` is overridden to plain JSON
(non-ASCII kept, no HTML escaping, UUIDs/datetimes serialised); ``tojson(2)`` indents.
"""

from __future__ import annotations

import json
import logging
import threading
from pathlib import Path
from typing import Any

import jinja2
import yaml

from memsvc.schemas.prompts import PromptSpec, RenderedPrompt

log = logging.getLogger(__name__)

_lock = threading.Lock()
_prompts: dict[str, dict[str, PromptSpec]] = {}  # name -> version -> spec
_loaded = False


def _tojson(value: Any, indent: int | None = None) -> str:
    return json.dumps(value, default=str, ensure_ascii=False, indent=indent)


_env = jinja2.Environment(undefined=jinja2.StrictUndefined, autoescape=False, keep_trailing_newline=True)
_env.filters["tojson"] = _tojson


def _version_key(version: str) -> int:
    return int(version.lstrip("v"))


def load(prompts_dir: Path) -> None:
    """(Re)load every prompt file in ``prompts_dir``. Called at app/worker startup.

    Raises ValueError for an invalid file or a duplicate ``name@version``; a missing directory loads nothing.
    Every template is compiled here so syntax errors surface at startup, not at lease time.
    """
    global _loaded
    found: dict[str, dict[str, PromptSpec]] = {}
    directory = Path(prompts_dir)
    files = sorted([*directory.glob("*.yaml"), *directory.glob("*.yml")]) if directory.is_dir() else []
    for path in files:
        try:
            data = yaml.safe_load(path.read_text(encoding="utf-8"))
            spec = PromptSpec.model_validate(data)
            _env.from_string(spec.user_template)
        except Exception as e:  # noqa: BLE001 - re-raised with the file name
            raise ValueError(f"invalid prompt file {path.name}: {e}") from e
        versions = found.setdefault(spec.name, {})
        if spec.version in versions:
            raise ValueError(f"duplicate prompt {spec.prompt_ref} in {path.name}")
        versions[spec.version] = spec
    with _lock:
        _prompts.clear()
        _prompts.update(found)
        _loaded = True
    log.info("loaded %d prompt(s) from %s", sum(len(v) for v in found.values()), directory)


def _ensure_loaded() -> None:
    """Load from ``settings.prompts_dir`` on first use when no process startup hook did."""
    if not _loaded:
        from memsvc.config import get_settings

        load(get_settings().prompts_dir)


def names() -> list[str]:
    _ensure_loaded()
    return sorted(_prompts)


def versions(name: str) -> list[str]:
    """Loaded versions of ``name``, oldest first."""
    _ensure_loaded()
    return sorted(_prompts.get(name, {}), key=_version_key)


def get(name: str, version: str | None = None) -> PromptSpec:
    """Prompt by name and version; ``version=None`` = highest loaded version. ``name`` may also be a
    full ``name@version`` ref. Raises KeyError when unknown."""
    _ensure_loaded()
    if "@" in name:
        name, _, ref_version = name.partition("@")
        version = version or ref_version
    by_version = _prompts.get(name)
    if not by_version:
        raise KeyError(f"unknown prompt {name!r}")
    if version is None:
        return by_version[max(by_version, key=_version_key)]
    if version not in by_version:
        raise KeyError(f"unknown prompt version {name}@{version}")
    return by_version[version]


def render(prompt: PromptSpec, inputs: dict[str, Any]) -> RenderedPrompt:
    """Render ``user_template`` with the packet ``inputs`` (StrictUndefined); ``system`` is returned as is.

    Raises ``jinja2.TemplateError`` (e.g. ``UndefinedError``) when the template does not fit the inputs.
    """
    user = _env.from_string(prompt.user_template).render(**inputs)
    return RenderedPrompt(system=prompt.system, user=user)
