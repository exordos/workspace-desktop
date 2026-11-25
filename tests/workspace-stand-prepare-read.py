"""Готовит только личный чат стендовых A/B к GPUI-сценарию чтения."""

import sys
import time
import uuid

sys.path.insert(0, "/stand")
from identities import USERS
from probe import MESSENGER, ORIGIN, login


def prepare():
    """Отмечает прежние сообщения B в чате A/B и ждёт пустого непрочитанного."""
    with login("B") as http:
        response = http.post(
            f"{ORIGIN}{MESSENGER}/streams/",
            json={"name": "Stand A / B", "direct_user_uuid": USERS["stand-a"]["uuid"]},
            timeout=5,
        )
        response.raise_for_status()
        stream = str(uuid.UUID(response.json()["uuid"]))
        response = http.post(
            f"{ORIGIN}{MESSENGER}/streams/{stream}/actions/read/invoke", timeout=5
        )
        response.raise_for_status()
        deadline = time.monotonic() + 15
        while True:
            response = http.get(
                f"{ORIGIN}{MESSENGER}/messages/",
                params={"stream_uuid": stream, "read": "false", "page_limit": 1},
                timeout=5,
            )
            response.raise_for_status()
            if not response.json():
                return
            if time.monotonic() >= deadline:
                raise RuntimeError("Старые непрочитанные в чате A/B не погашены")
            time.sleep(0.1)


if __name__ == "__main__":
    prepare()
