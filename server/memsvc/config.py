"""Process settings, read from the environment (no prefix: ``DATABASE_URL``, ``TEI_EMBED_URL`` ...).

Use :func:`get_settings` everywhere; it is cached per process. Tests set the environment in
``tests/conftest.py`` before the first call.
"""

from __future__ import annotations

from functools import lru_cache
from pathlib import Path

from pydantic_settings import BaseSettings, SettingsConfigDict

SERVER_DIR = Path(__file__).resolve().parent.parent


class Settings(BaseSettings):
    model_config = SettingsConfigDict(env_file=None, extra="ignore", case_sensitive=False)

    # --- database (spec 4.7, 14) ---
    # API/MCP pool, role memsvc_app. RLS applies.
    database_url: str = "postgresql://memsvc_app@127.0.0.1:5432/memsvc"
    # Broker/planner/migrations pool, role memsvc_system (BYPASSRLS). None = no system pool:
    # system_tx() then raises. Tests point both URLs at the same superuser DSN.
    database_system_url: str | None = None
    db_pool_min: int = 1
    db_pool_max: int = 10
    migrations_dir: Path = SERVER_DIR / "migrations"

    # --- embeddings and rerank (spec 13) ---
    tei_embed_url: str = "http://tei-embed:80"
    tei_rerank_url: str = "http://tei-rerank:80"
    embed_model: str = "bge-m3"  # value stored in embeddings.model
    embed_dims: int = 1024
    rerank_model: str = "bge-reranker-v2-m3"
    tei_embed_timeout_s: float = 2.0
    tei_rerank_timeout_s: float = 0.25  # spec 11.3
    # FAKE_TEI=1: use the deterministic in-process FakeTEI (tests, local dev without TEI).
    fake_tei: bool = False

    # --- prompts (spec 10.1) ---
    prompts_dir: Path = SERVER_DIR / "prompts"

    # --- auth (spec 5) ---
    token_pepper: str = ""  # HMAC key for token secret hashes; must be set in production
    rate_limit_events_per_min: int = 600  # events:write routes
    rate_limit_default_per_min: int = 120  # recall, MCP and everything else

    # --- ingest / packets (spec 9) ---
    window_tool_uses: int = 8
    window_idle_seconds: int = 90
    lease_seconds_default: int = 600
    preferred_runner_seconds: int = 600

    # --- worker (spec 13, 14, 4.8) ---
    worker_sweep_interval_s: float = 30.0
    embed_batch_size: int = 64
    embed_concurrency: int = 2
    embed_max_part_tokens: int = 512
    context_rerender_debounce_s: float = 2.0
    retention_events_payload_days: int = 30
    retention_packets_days: int = 90
    retention_logs_days: int = 180

    # --- HTTP server ---
    host: str = "0.0.0.0"
    port: int = 8000
    log_level: str = "info"


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    return Settings()
