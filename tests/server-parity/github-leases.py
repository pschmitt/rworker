"""Exercise dispatch, signed runner claims and cancellation with fake CI providers."""
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


def exercise(binary, root, provider, runner_pin=None):
    gitlab = provider == "gitlab"
    runner = "gl-linux-x64" if gitlab else "linux-arm64"
    profile = "gl-x86_64" if gitlab else "gh-aarch64"
    token_file = root / "token"
    token_file.write_text("fake-provider-credential")
    config = root / "config.json"
    state = root / "leases"
    settings = {
        "githubActions": {
            "repository": "example/project",
            "repositoryId": "123456",
            "tokenFile": str(token_file),
            "stateDirectory": str(state),
        },
    }
    if gitlab:
        settings = {"gitlabCi": {
            "instance": "https://gitlab.example.com",
            "project": "example/project",
            "projectId": 123456,
            "tokenFile": str(token_file),
            "stateDirectory": str(state),
        }}
    if runner_pin is not None:
        settings["gitlabCi"]["runnerId"] = runner_pin
    config.write_text(json.dumps(settings))
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
elif url.endswith(("/jwks", "/oauth/discovery/keys")):
    print((root / "jwks").read_text())
elif url.endswith("/dispatches"):
    dispatch = json.loads(json.loads(config["data-raw"]))
    assert dispatch["return_run_details"] is True
    assert dispatch["ref"] == "main"
    assert dispatch["inputs"]["profile"] == "linux-arm64"
    assert set(dispatch["inputs"]) == {"job_id", "profile"}
    print('{"workflow_run_id": 42}')
elif url.endswith("/runs/42"):
    cancelled = (root / "cancelled").exists()
    print(json.dumps({
        "status": "completed" if cancelled else "in_progress" if (root / "started").exists() else "queued",
        "conclusion": "cancelled" if cancelled else None,
    }))
elif url == "https://gitlab.example.com/api/v4/projects/123456/pipeline":
    dispatch = json.loads(json.loads(config["data-raw"]))
    assert dispatch["ref"] == "main"
    assert dispatch["inputs"]["profile"] == "gl-linux-x64"
    assert set(dispatch["inputs"]) == {"job_id", "profile"}
    print('{"id": 42}')
elif url.endswith("/pipelines/42"):
    print(json.dumps({"status": "canceled" if (root / "cancelled").exists() else
                     "running" if (root / "started").exists() else "pending"}))
elif url.endswith(("/runs/42/cancel", "/pipelines/42/cancel")):
    (root / "cancelled").touch()
else:
    sys.exit(22)
""")
    fake.chmod(0o700)
    console_key = "ssh-ed25519 " + base64.b64encode(bytes([1]) * 32).decode()
    (root / "rworker-web-worker-public-key").write_text(console_key + "\n")
    environment = dict(
        os.environ, PATH=f"{root}:{os.environ['PATH']}", RWORKER_GITHUB_FIXTURE=str(root),
        CREDENTIALS_DIRECTORY=str(root), RWORKER_WEB_WORKER_OWNER="operator",
    )
    command = [binary, "server", config]

    def control(*args):
        return json.loads(subprocess.check_output([*command, *args], env=environment))

    if gitlab:
        profiles = control("profiles")
        assert [row["name"] for row in profiles] == [profile]
        assert profiles[0]["image_tags"] == ["gitlab-ci"]
        cache = root / "inventory"
        cache.mkdir()
        (cache / "profiles.json").write_text(json.dumps([{"name": "standard"}]))
        cached = json.loads(subprocess.check_output(
            [*command, "profiles"],
            env=dict(environment, RWORKER_INVENTORY_DIRECTORY=str(cache)),
        ))
        assert [row["name"] for row in cached] == ["standard", profile]
    else:
        for resource_spec in ((4, 16), (2, 8)):
            profiles = control("profiles")
            assert {profile["name"] for profile in profiles} == {"gh-aarch64", "gh-x86_64"}
            for row in profiles:
                assert (row["vcpus"], row["ram_gb"]) == resource_spec
                assert row["image_tags"] == ["github-actions"]
            (root / "private").touch()

    public_key = "ssh-ed25519 " + base64.b64encode(bytes(32)).decode()
    lease = control(f"{provider}-up", runner, "2m", public_key, "operator@fixture", "fixture")
    assert lease["run_id"] == 42 and lease["state"] == "queued"
    ident = lease["id"]
    agent_id = f"glci-{ident}" if gitlab else ident
    if gitlab:
        assert lease["run_url"] == "https://gitlab.example.com/example/project/-/pipelines/42"
    assert control(f"{provider}-status", ident)["workflow"]["status"] == "queued"
    (root / "started").touch()
    assert control(f"{provider}-status", ident)["workflow"]["status"] == "in_progress"
    api = root / "agent.sock"

    def jwt(run_id, **overrides):
        now = int(time.time())
        claims = dict(
            iss="https://token.actions.githubusercontent.com", aud="rworker-job",
            nbf=now - 30, exp=now + 60, repository="example/project",
            repository_id="123456", workflow_ref="example/project/.github/workflows/rworker-job.yaml@refs/heads/main",
            ref="refs/heads/main", event_name="workflow_dispatch", runner_environment="github-hosted",
            run_id=str(run_id), run_attempt="1",
        )
        if gitlab:
            claims = dict(
                iss="https://gitlab.example.com", aud="rworker-job",
                nbf=now - 30, exp=now + 60, job_project_id="123456",
                job_project_path="example/project", pipeline_id=str(run_id),
                pipeline_source="api", ref_path="refs/heads/main", ref_protected="true",
                runner_id=99,
                ci_config_ref_uri="gitlab.example.com/example/project//.gitlab-ci.yml@refs/heads/main",
            )
        claims.update(overrides)
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
            f"POST /v1/agent/jobs/{agent_id}/{route} HTTP/1.1\r\n"
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
            details = {"profile": runner, "architecture": "x86_64" if gitlab else "aarch64",
                       "capabilities": ["kvm"]}
            disk = {"total_bytes": 145 << 30}
            details["disk"] = disk
            assert post("claim", jwt(41), details)[0] == 403
            if gitlab:
                for wrong in ({"runner_id": 0}, {"job_project_id": "9876"},
                              {"ref_protected": "false"}, {"pipeline_source": "push"}):
                    assert post("claim", jwt(42, **wrong), details)[0] == 403
                if runner_pin is not None:
                    assert post("claim", jwt(42, runner_id=100), details)[0] == 403
            runner_id = 100 if gitlab and runner_pin is None else 99
            status, result = post("claim", jwt(42, runner_id=runner_id) if gitlab else jwt(42), details)
            assert status == 200
            assert result["public_key"].splitlines() == [
                public_key,
                "no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-user-rc " + console_key,
            ]
            profiles = {profile["name"]: profile for profile in control("profiles")}
            assert profiles[profile]["disk_total_bytes"] == disk["total_bytes"]
            if not gitlab:
                assert "disk_total_bytes" not in profiles["gh-x86_64"]
            bearer = result["token"]
            assert post("claim", jwt(42), details)[0] == 403
            endpoint = {"port": 22223, "host_key": public_key}
            assert post("ready", "0" * 64, endpoint)[0] == 403
            assert post("ready", bearer, endpoint)[0] == 204
            ready = control(f"{provider}-status", ident)
            assert ready["state"] == "ready" and ready["endpoint"] == endpoint and ready["kvm"]
            assert "token_hash" not in ready
            assert control(f"{provider}-down", ident, "operator@fixture")["state"] == "cancelling"
            assert post("heartbeat", bearer, {})[0] == 403
            ended = control(f"{provider}-status", ident)
            assert ended["state"] == "ended" and ended["workflow"]["conclusion"] == ("canceled" if gitlab else "cancelled")

            (root / "cancelled").unlink()
            (root / "started").unlink()
            queued = control(f"{provider}-up", runner, "2m", public_key, "operator@fixture", "queued")
            record = state / queued["id"] / "lease.json"
            queued["expires_at"] = 1
            record.write_text(json.dumps(queued))
            control(f"{provider}-reap")
            assert (root / "cancelled").exists()
            assert control(f"{provider}-status", queued["id"])["state"] == "ended"
        finally:
            server.terminate()


if __name__ == "__main__":
    for provider, runner_pin in (("github", None), ("gitlab", None), ("gitlab", 99)):
        with tempfile.TemporaryDirectory() as directory:
            exercise(str(Path(sys.argv[1]).resolve()), Path(directory), provider, runner_pin)
