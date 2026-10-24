"""UUIDv7 ids (RFC 9562), generated server-side for every row.

Python 3.13's stdlib has no ``uuid.uuid7``. Layout: 48-bit Unix ms timestamp, 4-bit version (7),
12-bit counter (monotonic within one millisecond), 2-bit variant, 62 random bits.
"""

from __future__ import annotations

import os
import threading
import time
import uuid
from datetime import UTC, datetime

_lock = threading.Lock()
_last_ms = 0
_counter = 0


def uuid7() -> uuid.UUID:
    """Return a new time-ordered UUIDv7. Ids from one process are strictly increasing."""
    global _last_ms, _counter
    with _lock:
        ms = time.time_ns() // 1_000_000
        if ms > _last_ms:
            _last_ms = ms
            _counter = int.from_bytes(os.urandom(2)) & 0x3FF  # random start, leaves headroom
        else:
            _counter += 1
            if _counter > 0xFFF:  # counter exhausted in this ms: borrow the next ms
                _last_ms += 1
                _counter = 0
            ms = _last_ms
        counter = _counter
    rand_b = int.from_bytes(os.urandom(8)) & ((1 << 62) - 1)
    value = (ms & ((1 << 48) - 1)) << 80
    value |= 0x7 << 76
    value |= counter << 64
    value |= 0b10 << 62
    value |= rand_b
    return uuid.UUID(int=value)


def uuid7_time(u: uuid.UUID) -> datetime:
    """Creation time embedded in a UUIDv7."""
    return datetime.fromtimestamp((u.int >> 80) / 1000, tz=UTC)


def short_id(u: uuid.UUID | str) -> str:
    """Short display id used in citation tags (`obs#a1b2c3`): last 6 hex chars (random part)."""
    return str(u).replace("-", "")[-6:]
