"""Minimal SQL migration runner (replaces Alembic from spec 3/14).

Files ``server/migrations/NNNN_name.sql`` are applied in numeric order, each inside its own
transaction, and recorded in ``schema_migrations``. A session advisory lock serialises concurrent
runners (two API pods starting at once). Migrations are additive only: never edit an applied file,
add a new one in your module's number range (CONTRACTS.md).

After the files, the runner re-grants table and sequence privileges in schema ``public`` to
``memsvc_app`` and ``memsvc_system`` so tables added by any module are usable by both roles
(RLS still decides which rows ``memsvc_app`` sees).
"""

from __future__ import annotations

import hashlib
import logging
import re
from dataclasses import dataclass
from pathlib import Path

import asyncpg

log = logging.getLogger(__name__)

FILENAME_RE = re.compile(r"^(\d{4})_([a-z0-9_]+)\.sql$")
LOCK_KEY = 0x6D656D737663  # "memsvc"

GRANTS_SQL = """
GRANT USAGE ON SCHEMA public TO memsvc_app, memsvc_system;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO memsvc_app, memsvc_system;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO memsvc_app, memsvc_system;
"""


@dataclass(frozen=True)
class Migration:
    version: int
    name: str
    path: Path

    @property
    def sql(self) -> str:
        return self.path.read_text(encoding="utf-8")

    @property
    def checksum(self) -> str:
        return hashlib.sha256(self.path.read_bytes()).hexdigest()


def discover(migrations_dir: Path) -> list[Migration]:
    found: dict[int, Migration] = {}
    for p in sorted(migrations_dir.glob("*.sql")):
        m = FILENAME_RE.match(p.name)
        if not m:
            raise ValueError(f"bad migration filename {p.name!r} (want NNNN_name.sql)")
        version = int(m.group(1))
        if version in found:
            raise ValueError(f"duplicate migration number {version:04d}: {found[version].path.name}, {p.name}")
        found[version] = Migration(version, m.group(2), p)
    return [found[v] for v in sorted(found)]


async def apply_migrations(conn: asyncpg.Connection, migrations_dir: Path) -> list[int]:
    """Apply pending migrations on ``conn`` (must not be inside a transaction). Returns versions applied."""
    migrations = discover(migrations_dir)
    await conn.execute("SELECT pg_advisory_lock($1)", LOCK_KEY)
    try:
        await conn.execute(
            """CREATE TABLE IF NOT EXISTS schema_migrations (
                 version int PRIMARY KEY, name text NOT NULL, checksum text NOT NULL,
                 applied_at timestamptz NOT NULL DEFAULT now())"""
        )
        applied = {
            r["version"]: r["checksum"] for r in await conn.fetch("SELECT version, checksum FROM schema_migrations")
        }
        done: list[int] = []
        for m in migrations:
            if m.version in applied:
                if applied[m.version] != m.checksum:
                    log.warning("migration %04d_%s changed after it was applied", m.version, m.name)
                continue
            log.info("applying migration %04d_%s", m.version, m.name)
            async with conn.transaction():
                await conn.execute(m.sql)
                await conn.execute(
                    "INSERT INTO schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
                    m.version,
                    m.name,
                    m.checksum,
                )
            done.append(m.version)
        async with conn.transaction():
            await conn.execute(GRANTS_SQL)
        return done
    finally:
        await conn.execute("SELECT pg_advisory_unlock($1)", LOCK_KEY)


async def migrate(dsn: str, migrations_dir: Path) -> list[int]:
    """Connect to ``dsn`` and apply pending migrations."""
    conn = await asyncpg.connect(dsn)
    try:
        return await apply_migrations(conn, migrations_dir)
    finally:
        await conn.close()
