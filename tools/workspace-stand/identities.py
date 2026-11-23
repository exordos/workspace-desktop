"""Постоянные идентификаторы учебного проекта и двух аккаунтов."""

import uuid


def stand_uuid(name):
    """Получает один и тот же идентификатор для имени при каждом запуске."""
    return str(uuid.uuid5(uuid.NAMESPACE_DNS, f"workspace-desktop-stand.test/{name}"))


PROJECT = {"uuid": stand_uuid("project"), "name": "Workspace Stand",
           "organization": stand_uuid("organization")}
USERS = {
    f"stand-{letter.lower()}": {
        "uuid": stand_uuid(f"user-{letter.lower()}"),
        "name": f"Stand {letter}",
        "first_name": "Stand",
        "last_name": letter,
        "email": f"stand-{letter.lower()}@workspace-stand.test",
        "type": "human",
    }
    for letter in ("A", "B")
}
CLIENT = "/api/core/v1/iam/clients/default/actions"
DIRECTORY = "/api/core/v1/iam/users"
