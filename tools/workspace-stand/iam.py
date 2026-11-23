"""Локальный IAM для двух аккаунтов тестового Workspace."""

import hmac
import json
import os
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

import jwt
from gcl_iam import algorithms

from identities import CLIENT, DIRECTORY, PROJECT, USERS

AUDIENCE = "workspace-stand"
ISSUER = "urn:workspace-desktop:stand"
PRIVATE_KEY = Path("/keys/jwt.key").read_text()
PUBLIC_KEY = algorithms.generate_rsa_public_key_pem(PRIVATE_KEY)
JWK = algorithms.public_pem_to_jwk(PUBLIC_KEY)
PASSWORDS = {
    login: os.environ[f"STAND_PASSWORD_{letter}"]
    for login, letter in (("stand-a", "A"), ("stand-b", "B"))
}
USERS_BY_ID = {user["uuid"]: user for user in USERS.values()}


def decode(token, kind):
    """Проверяет подпись, срок, получателя и тип тестового токена."""
    claims = jwt.decode(token, PUBLIC_KEY, algorithms=["RS256"],
                        audience=AUDIENCE, issuer=ISSUER,
                        options={"require": ["exp", "sub", "typ"]})
    if claims["typ"] != kind or claims["sub"] not in USERS_BY_ID:
        raise jwt.InvalidTokenError()
    return claims


def tokens(user_id, project):
    """Выдаёт токены входа или выбранного проекта с проверяемым сроком."""
    now = int(time.time())
    access_ttl = 3600
    claims = {
        "aud": AUDIENCE, "iss": ISSUER, "sub": user_id, "project_id": project,
        "organization_id": PROJECT["organization"], "iat": now, "auth_time": now,
    }

    def issue(kind, ttl):
        return jwt.encode({**claims, "typ": kind, "exp": now + ttl, "jti": str(uuid.uuid4())},
                          PRIVATE_KEY, algorithm="RS256")

    return {
        "access_token": issue("access", access_ttl),
        "refresh_token": issue("refresh", 30 * 24 * 3600),
        "expires_in": access_ttl,
    }


class Handler(BaseHTTPRequestHandler):
    """Обслуживает контракт входа Desktop и проверки токенов бэкендом."""

    def respond(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlsplit(self.path).path.rstrip("/")
        if path == f"{CLIENT}/jwks":
            self.respond(200, {"algorithm": "RS256", "keys": [JWK]})
            return
        try:
            scheme, token = self.headers.get("Authorization", "").split()
            if scheme.lower() != "bearer":
                raise jwt.InvalidTokenError()
            claims = decode(token, "access")
        except (ValueError, jwt.InvalidTokenError):
            self.respond(401, {"error": "invalid_token"})
            return

        if path == f"{CLIENT}/introspect":
            self.respond(200, {
                "user_info": USERS_BY_ID[claims["sub"]],
                "project_id": claims["project_id"],
                "organization_id": PROJECT["organization"],
                "permissions": ["workspace.*.*"],
                "otp_verified": False,
            })
        elif path == "/api/core/v1/iam/projects":
            self.respond(200, [PROJECT])
        elif path == DIRECTORY:
            self.respond(200, list(USERS_BY_ID.values()))
        elif path.startswith(f"{DIRECTORY}/") and path.rsplit("/", 1)[-1] in USERS_BY_ID:
            user = USERS_BY_ID[path.rsplit("/", 1)[-1]]
            self.respond(200, {**user, "username": user["email"].split("@")[0]})
        else:
            self.respond(404, {"error": "not_found"})

    def do_POST(self):
        if urlsplit(self.path).path != f"{CLIENT}/get_token/invoke":
            self.respond(404, {"error": "not_found"})
            return
        try:
            length = int(self.headers.get("Content-Length", 0))
            if not 0 < length <= 8192:
                raise ValueError()
            body = json.loads(self.rfile.read(length))
            if not isinstance(body, dict):
                raise ValueError()
            scope = body.get("scope", "")
            if not isinstance(scope, str):
                raise ValueError()
        except (ValueError, UnicodeDecodeError):
            self.respond(400, {"error": "invalid_request"})
            return

        grant = body.get("grant_type")
        if grant == "login+password":
            login, password = body.get("login"), body.get("password")
            if (not isinstance(login, str) or login not in USERS
                    or not isinstance(password, str)
                    or not hmac.compare_digest(password.encode(), PASSWORDS[login].encode())):
                self.respond(401, {"error": "invalid_credentials"})
                return
            user_id, project = USERS[login]["uuid"], None
        elif grant == "refresh_token":
            try:
                claims = decode(body.get("refresh_token"), "refresh")
            except jwt.InvalidTokenError:
                self.respond(401, {"error": "invalid_grant"})
                return
            user_id, project = claims["sub"], claims["project_id"]
        else:
            self.respond(400, {"error": "unsupported_grant_type"})
            return

        requested = [part for part in scope.split() if part.startswith("project:")]
        if requested:
            if requested != [f"project:{PROJECT['uuid']}"] or grant != "refresh_token":
                self.respond(403, {"error": "invalid_scope"})
                return
            project = PROJECT["uuid"]
        self.respond(200, tokens(user_id, project))


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
