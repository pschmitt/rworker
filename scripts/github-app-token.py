"""Mint a GitHub App installation token limited to one repository.

Usage: github-app-token.py APP_ID_FILE PRIVATE_KEY_FILE OWNER/REPOSITORY

Prints the token on stdout. The token is scoped to the repository and to
`actions: write`, and GitHub expires it after one hour. Needs only the Python
standard library and the `openssl` command.
"""
import base64
import json
import subprocess
import sys
import time
from pathlib import Path
from urllib.request import Request, urlopen

API = "https://api.github.com"
API_VERSION = "2026-03-10"


def segment(value):
    return base64.urlsafe_b64encode(value).rstrip(b"=")


def request(path, credential, payload=None):
    body = None if payload is None else json.dumps(payload).encode()
    headers = {
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {credential}",
        "Content-Type": "application/json",
        "X-GitHub-Api-Version": API_VERSION,
    }
    query = Request(f"{API}{path}", data=body, headers=headers)
    with urlopen(query, timeout=15) as response:
        return json.load(response)


def app_jwt(app_id, key_path, now):
    signed = b".".join(
        segment(json.dumps(item).encode())
        for item in [
            {"typ": "JWT", "alg": "RS256"},
            {"iat": now - 45, "exp": now + 300, "iss": app_id},
        ]
    )
    signature = subprocess.check_output(
        ["openssl", "dgst", "-sha256", "-sign", key_path], input=signed
    )
    return (signed + b"." + segment(signature)).decode()


def mint(app_id_path, key_path, repository):
    owner, _, name = repository.partition("/")
    if not owner or not name or "/" in name:
        raise ValueError("Expected a repository as OWNER/NAME")
    app_id = Path(app_id_path).read_text().strip()
    if not (app_id.isascii() and app_id.isdecimal()):
        raise ValueError("Expected a numeric GitHub App ID")
    identity = app_jwt(app_id, key_path, int(time.time()))
    installation = request(f"/repos/{repository}/installation", identity)
    installation_id = int(installation["id"])
    issued = request(
        f"/app/installations/{installation_id}/access_tokens",
        identity,
        {"repositories": [name], "permissions": {"actions": "write"}},
    )
    token = issued["token"]
    if issued["permissions"].get("actions") != "write" or not token.startswith("ghs_"):
        raise ValueError("GitHub returned an unexpected installation token")
    return token


def main(argv):
    if len(argv) != 3:
        print(
            "Usage: github-app-token.py APP_ID_FILE PRIVATE_KEY_FILE OWNER/REPOSITORY",
            file=sys.stderr,
        )
        return 2
    try:
        print(mint(*argv), flush=True)
    except Exception as error:
        # Never print the exception text: it could carry credentials.
        print(
            f"Failed to mint the GitHub Actions credential ({type(error).__name__})",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
