"""Exercise the shared inventory refresher and its HTTP read API with fake providers."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def verify(binary, root):
    credentials = root / "credentials.json"
    credentials.write_text(json.dumps({"fake": {"data": dict.fromkeys(
        ["auth_url", "region", "application_credential_id", "application_credential_secret"], "fake"
    )}}))
    token = root / "token"
    token.write_text("fake")
    settings = root / "settings.json"
    settings.write_text(json.dumps({
        "adhocPrefix": "example-", "poolPrefix": "pool-", "openstackSecretName": "fake",
        "openstackSecretFile": str(credentials), "netbirdTokenFile": str(token),
        "netbirdApi": "https://example.invalid",
    }))
    calls = root / "calls"
    fixture = json.dumps([{"Name": "example-worker", "Status": "ACTIVE", "Properties": {}}])
    for name, program in {
        "curl": "printf '[]\\n'\n",
        "openstack": f"printf 'read\\n' >> '{calls}'\nprintf '%s\\n' '{fixture}'\n",
    }.items():
        path = root / name
        path.write_text(f"#!/bin/sh\n" + program)
        path.chmod(0o700)
    env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", RWORKER_INVENTORY_DIRECTORY=str(root))
    command = [str(binary), "server", str(settings)]
    socket = root / "http.sock"

    def ls(*args):
        return json.loads(subprocess.check_output([*command, "ls", *args], env=env, timeout=15))

    def request(path, *args):
        response = subprocess.check_output([
            "curl", "--unix-socket", str(socket), "-si", *args, f"http://localhost{path}"
        ], timeout=5).decode()
        return response.split("\r\n\r\n", 1)

    with subprocess.Popen([*command, "refresh-inventory"], env=env) as cache:
        with subprocess.Popen([*command, "inventory-api", str(socket)], env=env) as api:
            try:
                workers = ls()
                assert workers[0]["name"] == "example-worker", workers
                with ThreadPoolExecutor(8) as pool:
                    assert all(x == workers for x in pool.map(lambda _: ls(), range(12)))
                assert calls.read_text().splitlines() == ["read"]
                assert ls("--fresh") == workers
                assert calls.read_text().splitlines() == ["read", "read"]
                headers, body = request("/v1/workers")
                assert json.loads(body) == workers
                tag = next(line.split(": ", 1)[1] for line in headers.splitlines() if line.startswith("etag:"))
                assert "x-rworker-inventory-age-ms:" in headers
                conditional = request("/v1/workers", "-H", f"If-None-Match: {tag}")[0]
                assert conditional.startswith("HTTP/1.1 304"), conditional
                assert json.loads(request("/v1/workers/example-worker")[1]) == workers[0]
                assert request("/v1/workers/other")[0].startswith("HTTP/1.1 404")
                cache.terminate()
                cache.wait(timeout=5)
                expired = time.time() - 31
                os.utime(root / "inventory.json", (expired, expired))
                assert request("/v1/workers")[0].startswith("HTTP/1.1 503")
            finally:
                api.terminate()
                if cache.poll() is None:
                    cache.terminate()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-inventory-") as directory:
        verify(Path(sys.argv[1]).resolve(strict=True), Path(directory))
    print("Shared inventory refresh and HTTP status, ETag and age checks passed")
