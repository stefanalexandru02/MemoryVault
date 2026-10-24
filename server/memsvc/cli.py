"""``memsvc`` command line: migrate, serve, worker, planner, admin, evals."""

from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from pathlib import Path
from uuid import UUID

from memsvc.config import get_settings


def _cmd_migrate(args: argparse.Namespace) -> int:
    from memsvc.migrate import migrate

    settings = get_settings()
    dsn = args.dsn or settings.database_system_url or settings.database_url
    applied = asyncio.run(migrate(dsn, Path(args.dir) if args.dir else settings.migrations_dir))
    print(f"applied {len(applied)} migration(s)" + (f": {', '.join(f'{v:04d}' for v in applied)}" if applied else ""))
    return 0


def _cmd_serve(args: argparse.Namespace) -> int:
    import uvicorn

    settings = get_settings()
    uvicorn.run(
        "memsvc.app:create_app",
        factory=True,
        host=args.host or settings.host,
        port=args.port or settings.port,
        workers=1,
        log_level=settings.log_level,
    )
    return 0


def _cmd_worker(args: argparse.Namespace) -> int:
    from memsvc.worker import run_worker

    asyncio.run(run_worker(get_settings()))
    return 0


def _cmd_planner(args: argparse.Namespace) -> int:
    from memsvc.consolidate.planner import run_planner
    from memsvc.db import connect

    async def run() -> None:
        db = await connect(get_settings())
        try:
            result = await run_planner(db, UUID(args.space_id) if args.space_id else None)
            print(result.model_dump_json(indent=2))
        finally:
            await db.close()

    asyncio.run(run())
    return 0


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    logging.basicConfig(level=get_settings().log_level.upper(), format="%(asctime)s %(levelname)s %(name)s %(message)s")
    try:
        # admin/evals own their argument parsing; pass everything after the subcommand through.
        if argv and argv[0] == "admin":
            from memsvc.consolidate.admin_cli import main as admin_main

            return admin_main(argv[1:])
        if argv and argv[0] == "evals":
            from memsvc.evals.cli import main as evals_main

            return evals_main(argv[1:])

        parser = argparse.ArgumentParser(prog="memsvc", description="MemoryVault server")
        sub = parser.add_subparsers(dest="cmd", required=True)
        p = sub.add_parser("migrate", help="apply SQL migrations")
        p.add_argument("--dsn", help="default: DATABASE_SYSTEM_URL, else DATABASE_URL")
        p.add_argument("--dir", help="migrations directory (default: MIGRATIONS_DIR)")
        p.set_defaults(fn=_cmd_migrate)
        p = sub.add_parser("serve", help="run the API (uvicorn, one worker)")
        p.add_argument("--host")
        p.add_argument("--port", type=int)
        p.set_defaults(fn=_cmd_serve)
        p = sub.add_parser("worker", help="run the broker/worker loop")
        p.set_defaults(fn=_cmd_worker)
        p = sub.add_parser("planner", help="run one consolidation planning pass")
        p.add_argument("--space-id")
        p.set_defaults(fn=_cmd_planner)
        sub.add_parser("admin", help="admin commands (memsvc admin --help)")
        sub.add_parser("evals", help="evaluation harness (memsvc evals --help)")
        args = parser.parse_args(argv)
        return args.fn(args)
    except NotImplementedError:
        print("memsvc: this command is not implemented yet", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
