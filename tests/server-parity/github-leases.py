"""Exercise dispatch, signed runner claims and cancellation with fake GitHub."""
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def exercise(binary, root):
    token_file = root / "token"
    token_file.write_text("fake-provider-credential")
    config = root / "config.json"
    state = root / "leases"
    config.write_text(json.dumps({
        "githubActions": {
            "repository": "example/project",
            "repositoryId": "123456",
            "tokenFile": str(token_file),
            "stateDirectory": str(state),
        },
    }))
    key = root / "signing.pem"
    subprocess.run(
        ["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", key],
        check=True, stderr=subprocess.DEVNULL,
    )
    modulus = subprocess.check_output(["openssl", "rsa", "-in", key, "-noout", "-modulus"], text=True)
    modulus = bytes.fromhex(modulus.strip().split("=", 1)[1])
    (root / "jwks").write_text(json.dumps({
        "keys": [{"kid": "fixture", "kty": "RSA", "n": b64(modulus), "e": "AQAB"}],
    }))
    fake = root / "curl"
    fake.write_text(f"#!{sys.executable}\n" + """
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["RWORKER_GITHUB_FIXTURE"])
config = dict(line.strip().split(" = ", 1) for line in sys.stdin if not line.startswith("header"))
url = json.loads(config["url"])
if url == "https://api.github.com/repos/example/project":
    print(json.dumps({"private": (root / "private").exists()}))
elif url.endswith("/jwks"):
    print((root / "jwks").read_text())
elif url.endswith("/dispatches"):
    dispatch = json.loads(json.loads(config["data-raw"]))
    assert dispatch["return_run_details"] is True
    assert dispatch["ref"] == "main"
    assert dispatch["inputs"]["profile"] == "linux-arm64"
    assert set(dispatch["inputs"]) == {"job_id", "profile"}
    print('{"workflow_run_id": 42}')
elif url.endswith("/runs/42"):
    print(json.dumps({"status": "completed" if (root / "cancelled").exists() else "in_progress"}))
elif url.endswith("/runs/42/cancel"):
    (root / "cancelled").touch()
else:
    sys.exit(22)
""")
    fake.chmod(0o700)
    environment = dict(os.environ, PATH=f"{root}:{os.environ['PATH']}", RWORKER_GITHUB_FIXTURE=str(root))
    command = [binary, "server", config]

    def control(*args):
        return json.loads(subprocess.check_output([*command, *args], env=environment))

    for resource_spec in ((4, 16), (2, 8)):
        profiles = control("profiles")
        assert {profile["name"] for profile in profiles} == {"gh-aarch64", "gh-x86_64"}
        for profile in profiles:
            assert (profile["vcpus"], profile["ram_gb"]) == resource_spec
            assert profile["disk_gb"] == 14
            assert profile["image_tags"] == ["github-actions"]
        (root / "private").touch()

    public_key = "ssh-ed25519 " + base64.b64encode(bytes(32)).decode()
    lease = control("github-up", "linux-arm64", "2m", public_key, "operator", "fixture")
    assert lease["run_id"] == 42 and lease["state"] == "queued"
    ident = lease["id"]
    api = root / "agent.sock"

    def jwt(run_id):
        now = int(time.time())
        claims = dict(
            iss="https://token.actions.githubusercontent.com", aud="rworker-job",
            nbf=now - 30, exp=now + 60, repository="example/project",
            repository_id="123456", workflow_ref="example/project/.github/workflows/rworker-job.yaml@refs/heads/main",
            ref="refs/heads/main", event_name="workflow_dispatch", runner_environment="github-hosted",
            run_id=str(run_id), run_attempt="1",
        )
        data = ".".join(b64(json.dumps(value).encode()) for value in (
            {"alg": "RS256", "kid": "fixture"}, claims,
        ))
        signature = subprocess.check_output(
            ["openssl", "dgst", "-sha256", "-sign", key], input=data.encode(),
        )
        return f"{data}.{b64(signature)}"

    def post(route, bearer, data):
        body = json.dumps(data).encode()
        request = (
            f"POST /v1/agent/jobs/{ident}/{route} HTTP/1.1\r\n"
            f"Host: localhost\r\nConnection: close\r\nAuthorization: Bearer {bearer}\r\n"
            f"Content-Length: {len(body)}\r\n\r\n"
        ).encode() + body
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(str(api))
            client.sendall(request)
            with client.makefile("rb") as stream:
                headers, _, response = stream.read().partition(b"\r\n\r\n")
        return int(headers.split()[1]), json.loads(response) if response else None

    with subprocess.Popen([*command, "github-agent-api", api], env=environment) as server:
        try:
            for _ in range(100):
                if api.exists():
                    break
                assert server.poll() is None
                time.sleep(0.05)
            details = {"profile": "linux-arm64", "architecture": "aarch64", "capabilities": ["kvm"]}
            assert post("claim", jwt(41), details)[0] == 403
            status, result = post("claim", jwt(42), details)
            assert status == 200 and result["public_key"] == public_key
            bearer = result["token"]
            assert post("claim", jwt(42), details)[0] == 403
            endpoint = {"port": 22223, "host_key": public_key}
            assert post("ready", "0" * 64, endpoint)[0] == 403
            assert post("ready", bearer, endpoint)[0] == 204
            ready = control("github-status", ident)
            assert ready["state"] == "ready" and ready["endpoint"] == endpoint and ready["kvm"]
            assert "token_hash" not in ready
            assert control("github-down", ident, "operator")["state"] == "cancelling"
            assert post("heartbeat", bearer, {})[0] == 403
            assert control("github-status", ident)["state"] == "ended"
        finally:
            server.terminate()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as directory:
        exercise(str(Path(sys.argv[1]).resolve()), Path(directory))
