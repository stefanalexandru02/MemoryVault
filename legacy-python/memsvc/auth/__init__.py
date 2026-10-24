"""Authentication and authorization (spec 5): tokens, scopes, identity, space resolution."""

from memsvc.auth.deps import authenticate_header, check_rate_limit, get_identity, require_scope
from memsvc.auth.identity import Identity
from memsvc.auth.scopes import ADMIN, EVENTS_WRITE, KIND_SCOPES, MEMORIES_READ, MEMORIES_WRITE, WORK_LEASE
from memsvc.auth.spaces import readable_space_ids, resolve_read_spaces, resolve_write_space, writable_space_ids
from memsvc.auth.tokens import MintedToken, mint_token, revoke_token, verify_token

__all__ = [
    "ADMIN",
    "EVENTS_WRITE",
    "KIND_SCOPES",
    "MEMORIES_READ",
    "MEMORIES_WRITE",
    "WORK_LEASE",
    "Identity",
    "MintedToken",
    "authenticate_header",
    "check_rate_limit",
    "get_identity",
    "mint_token",
    "readable_space_ids",
    "require_scope",
    "resolve_read_spaces",
    "resolve_write_space",
    "revoke_token",
    "verify_token",
    "writable_space_ids",
]
