"""Compare failed boot and resource cleanup through the server CLI."""
import json
import base64
import contextlib
import io
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock, patch


def compare(oracle, binary, root):
    with patch.dict(sys.modules, {"openstack": ModuleType("openstack")}):
        up = runpy.run_path(str(oracle))["cmd_up"]
    cfg = {
        "adhocPrefix": "example-", "adhocFlavor": "small", "adhocVolumeSize": 16,
        "adhocMaxTtl": 3600, "adhocMax": 4, "adhocDefaultTtl": 60,
        "bootTimeout": 10, "netbirdAdhocGroup": "workers",
        "profiles": {"standard": {"imageTags": ["worker"], "imageNamePattern": "^rworker-[0-9a-f]{12}$"}},
        "openstackSecretFile": str(root / "secret"), "openstackSecretName": "fake",
        "netbirdTokenFile": str(root / "token"), "netbirdApi": "https://overlay.example.invalid",
        "networkId": "test-network", "securityGroup": "test-group",
    }
    key = root / "signing.pem"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-out", str(key)], check=True)
    cfg.update({
        "stateDir": str(root), "sshKeyFile": str(root / "unused"), "sshUser": "builder",
        "niks3": {"signingKeyFile": str(key), "keyId": "fixture", "issuer": "fixture-issuer",
                  "audience": "fixture-audience", "serverUrl": "https://cache.example.invalid"},
    })
    (root / "cfg").write_text(json.dumps(cfg))
    (root / "token").write_text("fake")
    (root / "secret").write_text(json.dumps({"fake": {"data": dict.fromkeys(
        ["auth_url", "region", "application_credential_id", "application_credential_secret"], "fake"
    )}}))
    for exe, script in {
        "openstack": """
args = sys.argv[1:]
match args[:2]:
    case ["server", "list"]: data = []
    case ["flavor", "show"]: data = {"id": "flavor", "vcpus": 1, "ram": 512}
    case ["limits", "show"]:
        data = [{"Name": n, "Value": v} for n, v in dict(
            max_total_cores=10, total_cores_used=0, max_total_ram_size=4096,
            total_ram_used=0, max_total_instances=10, total_instances_used=0
        ).items()]
    case ["image", "list"]: data = [{"ID": "image"}, {"ID": "other-family"}]
    case ["image", "show"]:
        data = (
            {"name": "rworker-abcdef123456", "created_at": "2026-01-01"}
            if args[-1] == "image"
            else {"name": "rworker-circus-abcdef123457", "created_at": "2026-01-02"}
        )
    case ["server", "create"]:
        boot_device = args[args.index("--block-device") + 1]
        assert "boot_index=0,delete_on_termination=true" in boot_device
        assert "uuid=image," in boot_device, boot_device
        document = json.loads(Path(args[args.index("--user-data")+1]).read_text())
        record(["create", document])
        Path(os.environ["FAKE_NAME"]).write_text(args[-1])
        data = {"id": "server"}
    case ["server", "show"]: data = {"status": os.environ["FAKE_STATUS"]}
    case ["console", "log"]:
        print("NIXBOT-WORKER-HOSTKEY fake-host-key")
        sys.exit(0)
    case ["server", "set"]:
        record(["host-key", args[args.index("--property") + 1]])
        data = None
    case ["server", "unset"]:
        record(["forget-key", args[args.index("--property") + 1]])
        data = None
    case ["server", "delete"]:
        assert args[-1] == "server"
        record(["delete"])
        data = None
    case _: raise AssertionError(args[:2])
print(json.dumps(data))
""",
        "timeout": """
assert "ssh" in sys.argv and "StrictHostKeyChecking=yes" in sys.argv
assert "cat > " in sys.argv[-1]
Path(os.environ["FAKE_JWT"]).write_text(sys.stdin.read())
""",
        "curl": """
req = sys.stdin.read()
if "/groups" in req:
    print(json.dumps([{"name": "workers", "id": "group"}]))
elif "/setup-keys/key" in req:
    record(["revoke"])
elif "/setup-keys" in req:
    assert "one-off" in req
    print(json.dumps({"id": "key", "key": "fake-key"}))
elif "/peers" in req:
    print(json.dumps([{"name": Path(os.environ["FAKE_NAME"]).read_text(), "ip": "127.0.0.1"}]) if os.environ["FAKE_STATUS"] == "ACTIVE" else [])
else:
    raise AssertionError("unexpected overlay request")
""",
    }.items():
        path = root / exe
        path.write_text(f"#!{sys.executable}\n" + """
import json, os, sys
from pathlib import Path
def record(event):
    with open(os.environ["FAKE_TRACE"], "a") as trace:
        print(json.dumps(event), file=trace)
""" + script)
        path.chmod(0o700)

    detailed = subprocess.run(
        [str(binary), "server", str(root / "cfg"), "profiles", "--details", "standard"],
        capture_output=True, text=True, timeout=10,
        env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}"},
        check=True,
    )
    [profile] = json.loads(detailed.stdout)
    assert profile["name"] == "standard" and "image_id" not in profile
    assert "vcpus" not in profile

    console_key = "ssh-ed25519 " + base64.b64encode(bytes([1]) * 32).decode()
    (root / "rworker-web-worker-public-key").write_text(console_key + "\n")
    for status, token in [("ERROR", False), ("ACTIVE", False), ("ACTIVE", True)]:
        owner = "operator@fixture" if token else "other@fixture"
        cloud = Mock()
        cloud.servers.return_value = {}
        server = SimpleNamespace(id="server", metadata={})
        cloud.create.return_value = (server, "key")
        success = status == "ACTIVE"
        cloud.boot_status.return_value = ("fake-host-key", "127.0.0.1") if success else "error"
        cloud.stage = "ACTIVE · ready" if success else "ERROR"
        cloud.ips = {"netbird": "127.0.0.1"}
        stderr, stdout = io.StringIO(), io.StringIO()
        def install(_cfg, _cloud, _server, expires):
            return {"ok": True, "expires_at": expires, "server_url": cfg["niks3"]["serverUrl"]}

        with (contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(stdout),
              patch.dict(up.__globals__, {"install_niks3_token": install})):
            try:
                up(cfg, cloud, SimpleNamespace(
                    profile=None, flavor=None, disk=None, scratch=False, ttl=60,
                    ssh_key=["fake-ssh"], no_niks3_token=not token, owner=owner, meta=[],
                ))
            except SystemExit as result:
                assert not success
                message = str(result)

        trace = root / "trace"
        trace.write_text("")
        result = subprocess.run(
            [str(binary), "server", str(root / "cfg"), "up", "--ssh-key=fake-ssh", f"--owner={owner}",
             *([] if token else ["--no-niks3-token"])],
            capture_output=True, text=True, timeout=20, env={
                **os.environ, "PATH": f"{root}:{os.environ['PATH']}", "FAKE_TRACE": str(trace),
                "CREDENTIALS_DIRECTORY": str(root), "RWORKER_WEB_WORKER_OWNER": "operator",
                "FAKE_STATUS": status, "FAKE_JWT": str(root / "jwt"), "FAKE_NAME": str(root / "name"),
            },
        )
        assert stderr.getvalue() in result.stderr, result
        expected = [["create", {"netbird_setup_key": "fake-key", **cloud.create.call_args.args[5]}]]
        if token:
            expected[0][1]["agent_authorized_keys"] = [
                "fake-ssh",
                "no-agent-forwarding,no-port-forwarding,no-X11-forwarding,no-user-rc " + console_key,
            ]
        if success:
            cloud.netbird.delete_key.assert_called_once_with("key")
            cloud.os.compute.set_server_metadata.assert_called_once_with(server, host_key="fake-host-key")
            cloud.forget_setup_key_id.assert_called_once_with("server")
            assert result.returncode == 0, result
            before, after = json.loads(stdout.getvalue()), json.loads(result.stdout)
            if token:
                expected_token, actual_token = before.pop("niks3_token"), after.pop("niks3_token")
                assert abs(expected_token.pop("expires_at") - actual_token.pop("expires_at")) <= 10
                assert actual_token == expected_token
                header, claims, signature = (root / "jwt").read_text().split(".")
                decode = lambda segment: base64.urlsafe_b64decode(segment + "=" * (-len(segment) % 4))
                assert json.loads(decode(header)) == {"alg": "EdDSA", "typ": "JWT", "kid": "fixture"}
                payload = json.loads(decode(claims))
                assert payload["exp"] == after["expires_at"]
                assert payload["sub"] == f"rworker:{after['name']}"
                assert payload["iss"] == "fixture-issuer" and payload["aud"] == "fixture-audience"
                assert len(payload["jti"]) == 32
                (root / "signed").write_text(f"{header}.{claims}")
                (root / "signature").write_bytes(decode(signature))
                subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-inkey", str(key),
                                "-in", str(root / "signed"), "-sigfile", str(root / "signature")],
                               check=True, capture_output=True)
            for item in (before, after):
                name = item.pop("name")
                assert name.startswith("example-") and len(name) == len("example-") + 8
            expiry = after["expires_at"]
            assert abs(before.pop("expires_at") - after.pop("expires_at")) <= 10
            assert before == after, result
            expected.extend([["revoke"], ["host-key", "host_key=fake-host-key"], ["forget-key", "setup_key_id"]])
            if token:
                expected.append(["host-key", f"niks3_token_exp={expiry}"])
        else:
            cloud.delete.assert_called_once()
            assert cloud.delete.call_args.args[1] == "key"
            assert (result.returncode, result.stdout) == (1, ""), result
            assert result.stderr.splitlines()[-1].split(" ", 1)[1] == message.split(" ", 1)[1], result
            expected.extend([["revoke"], ["delete"]])
        assert [json.loads(line) for line in trace.read_text().splitlines()] == expected


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-up-") as directory:
        compare(*(Path(arg).resolve(strict=True) for arg in sys.argv[1:]), Path(directory))
    print("OpenStack boot parity passed and console key provisioning was scoped to the operator")
