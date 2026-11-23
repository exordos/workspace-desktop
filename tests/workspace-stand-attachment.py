"""Отправляет тестовое фото и проверяет его из второго аккаунта."""

import hashlib
import struct
import sys
import time
import uuid
import zlib

sys.path.insert(0, "/stand")
from identities import USERS
from probe import MESSENGER, ORIGIN, login


def image_bytes():
    """Создаёт синтетическое PNG для проверки файла и просмотра в Desktop."""
    def chunk(name, data):
        return struct.pack(">I", len(data)) + name + data + struct.pack(">I", zlib.crc32(name + data))

    width, height = 192, 128
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    colors = b"\x1c\x9c\x8c" * 64 + b"\xdd\xb2\x4c" * 64 + b"\xda\x54\x69" * 64
    pixels = (b"\x00" + colors) * height
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b"")


def result(response):
    """Возвращает JSON или показывает ответ реального API при ошибке."""
    if not response.ok:
        raise RuntimeError(f"{response.request.method} {response.url}: {response.status_code} {response.text}")
    return response.json()


def send():
    """Загружает фото из аккаунта A и отправляет сообщение в личный чат."""
    with login("A") as http:
        stream = result(http.post(f"{ORIGIN}{MESSENGER}/streams/", json={
            "name": "Stand A / B", "direct_user_uuid": USERS["stand-b"]["uuid"],
        }, timeout=5))
        photo = result(http.post(f"{ORIGIN}{MESSENGER}/files/",
                                  data={"stream_uuid": stream["uuid"]},
                                  files={"file": ("stand-probe.png", image_bytes(), "image/png")},
                                  timeout=5))
        message = result(http.post(f"{ORIGIN}{MESSENGER}/messages/", json={
            "uuid": str(uuid.uuid4()), "stream_uuid": stream["uuid"],
            "payload": {"kind": "markdown",
                        "content": f"Проверка локального стенда\n\n![stand-probe.png](urn:image:{photo['uuid']})"},
        }, timeout=5))
        print(message["uuid"])


def open_attachment(message):
    """Находит отправленное сообщение под B и сверяет байты файла из MinIO."""
    with login("B") as http:
        deadline = time.monotonic() + 15
        while True:
            response = http.get(f"{ORIGIN}{MESSENGER}/messages/{message}", timeout=5)
            if response.status_code != 404 or time.monotonic() >= deadline:
                body = result(response)
                break
            time.sleep(0.5)
        uri = body["payload"]["content"].split("urn:image:", 1)[1].split(")", 1)[0]
        photo = str(uuid.UUID(uri))
        response = http.get(f"{ORIGIN}{MESSENGER}/files/{photo}/actions/download", timeout=5)
        response.raise_for_status()
        expected = image_bytes()
        if response.content != expected or response.headers.get("ETag") != f'"{hashlib.sha256(expected).hexdigest()}"':
            raise RuntimeError("Вложение из истории B не совпало с отправленным файлом.")
        print(f"Аккаунт B открыл фото {photo} из сообщения {message} после перезапуска.")


if __name__ == "__main__":
    if sys.argv[1:] == ["send"]:
        send()
    else:
        open_attachment(str(uuid.UUID(sys.argv[2])))
