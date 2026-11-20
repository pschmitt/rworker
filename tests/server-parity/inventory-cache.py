"""Exercise the shared inventory refresher and its HTTP read API with fake providers."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def verify(binary, root, web_enabled):
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
    pause = root / "pause"
    fixture = json.dumps([{"Name": "example-worker", "Status": "ACTIVE", "Properties": {}}])
    for name, program in {
        "curl": "print('[]')",
        "openstack": f"with open({str(calls)!r}, 'a') as log:\n"
                     f"    print('read', file=log)\n"
                     f"from pathlib import Path\nimport time\n"
                     f"if Path({str(pause)!r}).exists():\n    time.sleep(0.5)\n"
                     f"print({fixture!r})",
    }.items():
        path = root / name
        path.write_text(f"#!{sys.executable}\n{program}\n")
        path.chmod(0o700)
    env = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}",
               RWORKER_INVENTORY_DIRECTORY=str(root), RWORKER_EVENT_DIRECTORY=str(root))
    web_token = root / "web-token"
    fixture_token = "fixture" * 6
    web_token.write_text(fixture_token)
    origin = "https://rworker.example.invalid"
    archive = root / "rworker" / "job-logs" / "example-worker"
    archive.mkdir(parents=True)
    (archive / "compile").write_text("cached build")
    env.update(RWORKER_WEB_TOKEN_FILE=str(web_token), RWORKER_WEB_ORIGIN=origin,
               RWORKER_WEB_PROXY_USER="operator", XDG_STATE_HOME=str(root),
               RWORKER_CONTROL_SOCKET=str(root / "control.sock"))
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
                events = json.loads(subprocess.check_output(
                    [*command, "lifecycle-events"], env=env, timeout=5
                ))
                assert [(row["kind"], row["target"]) for row in events] == [
                    ("worker-added", "example-worker")
                ]
                with ThreadPoolExecutor(8) as pool:
                    assert all(x == workers for x in pool.map(lambda _: ls(), range(12)))
                assert calls.read_text().splitlines() == ["read"]
                pause.touch()
                with ThreadPoolExecutor(1) as pool:
                    refreshing = pool.submit(ls, "--fresh")
                    deadline = time.monotonic() + 3
                    while not (root / "refresh").exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    assert (root / "refresh").exists()
                    assert json.loads(request("/v1/workers")[1]) == workers
                    assert refreshing.result() == workers
                pause.unlink()
                assert calls.read_text().splitlines() == ["read", "read"]
                assert json.loads(subprocess.check_output(
                    [*command, "lifecycle-events"], env=env, timeout=5
                )) == events
                headers, body = request("/v1/workers")
                assert json.loads(body) == workers
                tag = next(line.split(": ", 1)[1] for line in headers.splitlines() if line.startswith("etag:"))
                assert "x-rworker-inventory-age-ms:" in headers
                conditional = request("/v1/workers", "-H", f"If-None-Match: {tag}")[0]
                assert conditional.startswith("HTTP/1.1 304"), conditional
                assert json.loads(request("/v1/workers/example-worker")[1]) == workers[0]
                assert request("/v1/workers/other")[0].startswith("HTTP/1.1 404")
                if web_enabled:
                    headers, body = request("/")
                    assert headers.startswith("HTTP/1.1 200") and "content-security-policy:" in headers
                    assert "/assets/app.js" in body
                    assert request("/assets/app.js")[0].startswith("HTTP/1.1 200")
                    assert request("/v1/ui/events")[0].startswith("HTTP/1.1 401")
                    assert request("/v1/ui/session", "-H", "Remote-User: visitor")[0].startswith("HTTP/1.1 403")
                    assert request("/v1/ui/session", "-H", "Remote-User: operator")[0].startswith("HTTP/1.1 200")
                    jobs = json.loads(request("/v1/ui/jobs", "-H", "Remote-User: operator")[1])["data"]
                    assert jobs == [{"worker": "example-worker", "job": "compile", "state": "archived"}]
                    assert json.loads(request("/v1/ui/jobs/example-worker", "-H", "Remote-User: operator")[1])["data"] == jobs
                    auth = ("-H", f"Authorization: Bearer {fixture_token}")
                    assert request("/v1/ui/unknown", *auth)[0].startswith("HTTP/1.1 404")
                    action = (*auth, "-H", "Content-Type: application/json", "--data", '{"action":"unknown"}')
                    assert request("/v1/ui/action", *action)[0].startswith("HTTP/1.1 403")
                    assert request("/v1/ui/action", *action, "-H", f"Origin: {origin}")[0].startswith("HTTP/1.1 400")
                else:
                    assert request("/")[0].startswith("HTTP/1.1 404")
                    assert request("/assets/app.js")[0].startswith("HTTP/1.1 404")
                    assert request("/v1/ui/events")[0].startswith("HTTP/1.1 404")
                cache.terminate()
                cache.wait(timeout=5)
                expired = time.time() - 31
                os.utime(root / "inventory.json", (expired, expired))
                assert request("/v1/workers")[0].startswith("HTTP/1.1 503")
                if web_enabled:
                    stale = json.loads(request("/v1/ui/workers", "-H", "Remote-User: operator")[1])
                    assert stale == {"data": workers, "stale": True}, stale
            finally:
                api.terminate()
                if cache.poll() is None:
                    cache.terminate()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-inventory-") as directory:
        verify(Path(sys.argv[1]).resolve(strict=True), Path(directory), "--without-web" not in sys.argv[2:])
    print("Shared inventory refresh and HTTP status, ETag and age checks passed")
