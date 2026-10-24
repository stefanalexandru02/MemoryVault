"""Spec 10.1: prompt registry entries (``server/prompts/*.yaml``)."""

from __future__ import annotations

from pydantic import BaseModel, Field

from memsvc.schemas.common import OutputSchemaName


class PromptSpec(BaseModel):
    """One versioned prompt. ``prompt_ref = name@version``; packets pin the ref they were built with."""

    name: str = Field(pattern=r"^[a-z0-9_.-]+$")  # e.g. 'observer.code'
    version: str = Field(pattern=r"^v\d+$")  # e.g. 'v3'
    system: str
    user_template: str  # Jinja2, rendered with the packet inputs
    output_schema: OutputSchemaName
    model_hint: str = "sonnet"
    description: str | None = None

    @property
    def prompt_ref(self) -> str:
        return f"{self.name}@{self.version}"


class RenderedPrompt(BaseModel):
    system: str
    user: str


class PromptOut(BaseModel):
    """``GET /v1/prompts/{name}`` (memories:read)."""

    name: str
    version: str
    prompt_ref: str
    system: str
    user_template: str
    output_schema: OutputSchemaName
    model_hint: str
    versions: list[str] = Field(default_factory=list)  # every loaded version of this name
