"""Подготавливает проект и бакет вложений после миграций PostgreSQL."""

import time
from urllib.error import URLError
from urllib.request import urlopen
from uuid import UUID

import psycopg
from botocore.exceptions import ClientError
from oslo_config import cfg

from identities import PROJECT, USERS
from workspace.common import file_storage_opts
from workspace.messenger_api import file_storage
from workspace.messenger_api.dm.models import build_workspace_user_default_avatar


def prepare_database():
    """Регистрирует тестовых пользователей, сохраняя уже созданные данные."""
    with psycopg.connect("postgresql://workspace:local-workspace-db@db:5432/workspace") as db:
        for login, user in USERS.items():
            avatar = build_workspace_user_default_avatar(UUID(user["uuid"]))
            db.execute("""
                INSERT INTO m_workspace_users
                    (uuid, username, first_name, last_name, email, source, status, avatar,
                     last_ping_at, created_at, updated_at)
                VALUES (%s, %s, %s, %s, %s, 'iam', 'active', %s, NOW(), NOW(), NOW())
                ON CONFLICT (uuid) DO NOTHING
            """, (user["uuid"], login, user["first_name"], user["last_name"], user["email"], avatar))
            db.execute("SELECT messenger_v2_register_project_user(%s, %s)",
                       (PROJECT["uuid"], user["uuid"]))


def prepare_bucket():
    """Дожидается MinIO и создаёт бакет только при первом запуске."""
    deadline = time.monotonic() + 45
    while True:
        try:
            with urlopen("http://s3:9000/minio/health/ready", timeout=2):
                break
        except (URLError, TimeoutError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError("MinIO не стал доступен за 45 секунд") from error
            time.sleep(1)

    file_storage_opts.register_opts()
    cfg.CONF(["--config-file", "/stand/backend.conf"])
    storage = file_storage.S3WorkspaceFileStorage()
    try:
        storage.client.head_bucket(Bucket=storage.bucket_name)
    except ClientError as error:
        if error.response["ResponseMetadata"]["HTTPStatusCode"] != 404:
            raise
        storage.client.create_bucket(Bucket=storage.bucket_name)


if __name__ == "__main__":
    prepare_database()
    prepare_bucket()
