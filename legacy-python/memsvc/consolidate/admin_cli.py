"""``memsvc admin ...`` subcommands (spec 5.2, 6.7), for use via ``kubectl exec``. Owner: consolidation.

Expected commands include ``user create``, ``space create``, ``membership add``,
``token create --user alex --client laptop-x1 --kind device``, ``token revoke``, ``consolidate``,
``revert``, ``reindex``, ``project remap``. Bootstrap helpers: ``memsvc.auth.admin`` and
``memsvc.auth.tokens.mint_token``.
"""

from __future__ import annotations


def main(argv: list[str]) -> int:
    """Run an admin subcommand; ``argv`` is everything after ``memsvc admin``. Returns the exit code."""
    raise NotImplementedError
