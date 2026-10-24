from __future__ import annotations

import time
from datetime import UTC, datetime

from memsvc.ids import short_id, uuid7, uuid7_time


def test_uuid7_version_order_time() -> None:
    ids = [uuid7() for _ in range(5000)]
    assert all(u.version == 7 and u.variant == "specified in RFC 4122" for u in ids)
    assert ids == sorted(ids) and len(set(ids)) == len(ids)
    assert abs((uuid7_time(ids[0]) - datetime.now(UTC)).total_seconds()) < 2
    a = uuid7()
    time.sleep(0.002)
    assert uuid7() > a
    assert len(short_id(a)) == 6
