"""Проверяет доступность HTTPS, IAM, REST и WebSocket через общий вход стенда."""

import asyncio
import json
import os
import ssl
import sys
from urllib.parse import urlencode

import requests
import websockets

from identities import CLIENT, PROJECT, USERS

ORIGIN = "https://gateway"
CA = "/keys/ca.pem"
MESSENGER = "/api/workspace/v1/messenger"


def login(letter):
    """Входит в IAM и получает отдельный токен выбранного проекта."""
    http = requests.Session()
    http.verify = CA
    endpoint = f"{ORIGIN}{CLIENT}/get_token/invoke"
    response = http.post(endpoint, json={
        "grant_type": "login+password", "login": f"stand-{letter.lower()}",
        "password": os.environ[f"STAND_PASSWORD_{letter}"],
    }, timeout=3)
    response.raise_for_status()
    response = http.post(endpoint, json={
        "grant_type": "refresh_token", "refresh_token": response.json()["refresh_token"],
        "scope": f"openid email profile project:{PROJECT['uuid']}",
    }, timeout=3)
    response.raise_for_status()
    http.headers["Authorization"] = f"Bearer {response.json()['access_token']}"
    return http


async def ready(http, cursor):
    """Ожидает кадр ready настоящего сервера событий."""
    query = urlencode({"epoch_generation": cursor["epoch_generation"],
                       "last_epoch_version": cursor["epoch_version"]})
    async with websockets.connect(
        f"wss://gateway/api/workspace/v1/events/ws?{query}",
        subprotocols=["workspace.events.v1", http.headers["Authorization"].replace("Bearer ", "bearer.", 1)],
        ssl=ssl.create_default_context(cafile=CA), open_timeout=3,
    ) as socket:
        async with asyncio.timeout(4):
            while True:
                frame = json.loads(await socket.recv())
                if frame.get("type") == "ready":
                    return
                if frame.get("type") == "error":
                    raise RuntimeError(f"WebSocket отклонил курсор: {frame.get('error')}")


def check(prepare):
    """Проверяет REST и WSS, при первом пуске открывает личку A и B."""
    requests.get("http://s3:9000/minio/health/ready", timeout=3).raise_for_status()
    with login("A") as http:
        if prepare:
            response = http.post(f"{ORIGIN}{MESSENGER}/streams/", json={
                "name": "Stand A / B", "direct_user_uuid": USERS["stand-b"]["uuid"],
            }, timeout=5)
            response.raise_for_status()
        response = http.get(f"{ORIGIN}/api/workspace/v1/epoch/", timeout=3)
        response.raise_for_status()
        asyncio.run(ready(http, response.json()))


if __name__ == "__main__":
    check(sys.argv[1:] == ["prepare"])
