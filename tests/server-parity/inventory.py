"""Compare inventory JSON against the Python control host with fake providers."""
import contextlib
import io
import json
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
        legacy = runpy.run_path(str(oracle))
    settings = {
        "adhocPrefix": "example-", "poolPrefix": "pool-", "adhocGracePeriod": 600,
        "adhocMaxTtl": 10000, "openstackSecretName": "test",
        "openstackSecretFile": str(root / "secret"), "netbirdTokenFile": str(root / "token"),
        "netbirdApi": "https://netbird.example.invalid",
    }
    (root / "secret").write_text(json.dumps({"test": {"data": dict.fromkeys(
        ["auth_url", "region", "application_credential_id", "application_credential_secret"], "fake"
    )}}))
    (root / "token").write_text("fake")
    (root / "oauth").write_text(json.dumps({"client_id": "fake", "client_secret": "fake"}))
    settings["tailscale"] = {
        "oauthFile": str(root / "oauth"), "api": "https://tailscale.example.invalid",
        "tag": "tag:worker",
    }
    (root / "settings").write_text(json.dumps(settings))
    for name, body in {
        "openstack": """
import json, os, sys
operation = sys.argv[2]
if operation == "list":
    print(os.environ["FAKE_LIST"])
elif operation == "show":
    print(json.loads(os.environ["FAKE_DETAILS"])[sys.argv[-1]])
else:
    raise AssertionError(operation)
""",
        "curl": """
import json, os, sys
request = sys.stdin.read()
if "netbird.example.invalid/peers" in request:
    print(os.environ["FAKE_PEERS"])
elif "tailscale.example.invalid/oauth/token" in request:
    assert "grant_type=client_credentials" in request
    print(json.dumps({"access_token": "fake", "expires_in": 3600}))
elif "tailscale.example.invalid/tailnet/-/devices" in request:
    assert "Authorization: Bearer fake" in request
    print(os.environ["FAKE_DEVICES"])
else:
    raise AssertionError("unexpected request")
""",
    }.items():
        command = root / name
        command.write_text(f"#!{sys.executable}\n" + body)
        command.chmod(0o700)

    def worker(name, metadata, task_state=None):
        return SimpleNamespace(
            name=name, metadata=metadata, created_at="2026-01-01T00:00:00Z",
            task_state=task_state, status="ACTIVE",
            flavor={"original_name": "example.small"}, availability_zone="example",
        )

    ready = worker("example-ready", {
        "nixbot-worker": "adhoc", "agent_id": "example:agent", "setup_key_id": "hidden",
        "created_at": "1000", "expires_at": "1800", "last_active": "1900", "max_lifetime": "3600",
    })
    pool = worker("pool-old", {"nixbot-worker": "1"}, "deleting")
    peers = [{"name": ready.name, "ip": "192.0.2.1", "connected": True}]
    fallback = worker("example-fallback", {})
    device = {"hostname": fallback.name, "tags": ["tag:worker"],
              "addresses": ["2001:db8::1", "192.0.2.2"], "connectedToControl": True}
    cases = [([], [], []), ([pool, worker("unrelated", {}), ready], peers, []),
             ([fallback], [], [device])]
    for servers, peers, devices in cases:
        cloud = Mock()
        cloud.tailscale.devices.return_value = {dev["hostname"]: dev for dev in devices}
        cloud.os.compute.servers.return_value = servers
        cloud.netbird.call.return_value = peers
        cloud.overlay_ips.side_effect = lambda name, peers, devices: {
            "netbird": peers[name]["ip"]
        } if name in peers else (
            {"tailscale": legacy["tailscale_ip"](devices[name])} if name in devices else {}
        )
        expected = io.StringIO()
        with contextlib.redirect_stdout(expected):
            legacy["cmd_ls"](settings, cloud, None)
        details = {
            server.name: json.dumps({
                "properties": server.metadata, "status": server.status,
                "created": server.created_at, "flavor": server.flavor,
                "OS-EXT-STS:task_state": server.task_state,
                "OS-EXT-AZ:availability_zone": server.availability_zone,
            })
            for server in servers
        }
        actual = subprocess.run(
            [str(binary), "server", str(root / "settings"), "ls"],
            capture_output=True, text=True, timeout=10,
            env={
                **os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                "FAKE_LIST": json.dumps([{"ID": s.name, "Name": s.name} for s in servers]),
                "FAKE_DETAILS": json.dumps(details), "FAKE_PEERS": json.dumps(peers),
                "FAKE_DEVICES": json.dumps({"devices": devices}),
            },
        )
        assert (actual.returncode, actual.stderr) == (0, ""), actual
        assert json.loads(actual.stdout) == json.loads(expected.getvalue()), actual
    return len(cases)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-inventory-") as directory:
        count = compare(*(Path(arg).resolve(strict=True) for arg in sys.argv[1:]), Path(directory))
    print(f"Rust vs Python inventory: {count} cases compared, 0 differences")
