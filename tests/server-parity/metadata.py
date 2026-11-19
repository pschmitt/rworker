"""Characterize topic and claim against a fake compute service."""
import contextlib
import io
import json
import os
import subprocess
import tempfile
from pathlib import Path
import runpy
import sys
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock, call, patch


def characterize(oracle, binary=None, directory=None):
    with patch.dict(sys.modules, {"openstack": ModuleType("openstack")}):
        commands = runpy.run_path(str(oracle))
    cfg = {"adhocPrefix": "example-", "poolPrefix": "pool-"}
    worker = SimpleNamespace(metadata={"owner": "alice", "agent": "old", "topic": "old"})
    name = "example-test"
    cfg["adhocMaxTtl"] = 3600
    worker.metadata.update({"created_at": "1000", "max_lifetime": "1200"})


    if binary is not None:
        config = directory / "config.json"
        secret = directory / "credentials.json"
        trace = directory / "trace.jsonl"
        secret.write_text(json.dumps({"test": {"data": {
            "auth_url": "https://identity.example.invalid/v3",
            "application_credential_id": "fake",
            "application_credential_secret": "fake",
            "region": "example",
        }}}))
        token = directory / "token"
        token.write_text("fake")
        curl = directory / "curl"
        curl.write_text(f"#!{sys.executable}\nimport sys\nassert '/peers' in sys.stdin.read()\nprint('[]')\n")
        curl.chmod(0o700)
        config.write_text(json.dumps({
            **cfg, "openstackSecretFile": str(secret), "openstackSecretName": "test",
            "netbirdTokenFile": str(token), "netbirdApi": "https://overlay.example.invalid",
        }))
        fake = directory / "openstack"
        fake.write_text(f"#!{sys.executable}\n" + """
import json
import os
import sys

resource, action, *args = sys.argv[1:]
assert resource == "server"
if action == "list":
    print(json.dumps([{"ID": "fake-id", "Name": "example-test"}]))
elif action == "show":
    assert args[-1] == "fake-id"
    print(os.environ["RWORKER_FAKE_SERVER"])
elif action in ("set", "unset"):
    assert args[-2:] == ["--", "fake-id"]
    assert all(flag == "--property" for flag in args[:-2:2])
    values = args[1:-2:2]
    payload = dict(value.split("=", 1) for value in values) if action == "set" else values
    with open(os.environ["RWORKER_FAKE_TRACE"], "a") as trace:
        print(json.dumps([action, payload]), file=trace)
elif action == "delete":
    assert args == ["--", "fake-id"]
    with open(os.environ["RWORKER_FAKE_TRACE"], "a") as trace:
        print(json.dumps(["delete", "example-test"]), file=trace)
else:
    raise AssertionError(action)
""")
        fake.chmod(0o700)

    def compare(action, owner, force, kwargs, result, calls):
        if binary is None:
            return
        trace.write_text("")
        args = [str(binary), "server", str(config), action, name, "--owner", owner]
        if force:
            args.append("--force")
        for pair in kwargs.get("meta", []):
            args.append("--meta=" + pair)
        if "ttl" in kwargs:
            args.extend(["--ttl", str(kwargs["ttl"])])
        if kwargs.get("no_niks3_token"):
            args.append("--no-niks3-token")
        if action == "topic":
            args.extend(["--", kwargs["text"]])
        actual = subprocess.run(args, capture_output=True, text=True, timeout=10, env={
            **os.environ, "PATH": f"{directory}:{os.environ['PATH']}",
            "RWORKER_FAKE_SERVER": json.dumps({"properties": worker.metadata}),
            "RWORKER_FAKE_TRACE": str(trace),
        })
        if isinstance(result, str) and not result.startswith("deleted "):
            assert (actual.returncode, actual.stdout) == (1, ""), actual
            diagnostic = actual.stderr.strip()
            # Python repr uses single quotes for simple strings.
            if " as KEY=VALUE, got " in diagnostic and kwargs.get("meta") == ["invalid=one"]:
                diagnostic = diagnostic.removesuffix('"invalid=one"') + (
                    "'invalid=one'" if diagnostic.endswith('"invalid=one"') else ""
                )
            assert diagnostic == result, actual
        else:
            assert (actual.returncode, actual.stderr) == (0, ""), actual
            if action == "down":
                assert actual.stdout == result, actual
            else:
                assert json.loads(actual.stdout) == result, actual
        expected = [
            ["set", operation.kwargs] if operation[0] == "set_server_metadata"
            else ["delete", operation.args[0]] if operation[0] == "delete"
            else ["unset", operation.args[1]]
            for operation in calls
        ]
        assert [json.loads(line) for line in trace.read_text().splitlines()] == expected

    def invoke(action, owner="alice", force=False, **kwargs):
        cloud = Mock()
        cloud.servers.return_value = {name: worker}
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            try:
                commands[f"cmd_{action.replace('-', '_')}"](
                    cfg, cloud, SimpleNamespace(name=name, owner=owner, force=force, **kwargs)
                )
            except SystemExit as error:
                result = str(error)
            else:
                result = output.getvalue() if action == "down" else json.loads(output.getvalue())
        operations = cloud.os.compute.mock_calls + [call.delete(*c.args) for c in cloud.delete.mock_calls]
        compare(action, owner, force, kwargs, result, operations)
        return result, operations

    assert invoke("topic", text="  café\n\u200b now  ") == (
        {"name": name, "topic": "café now"},
        [call.set_server_metadata(worker, topic="café now")],
    )
    assert invoke("topic", text="\t") == (
        {"name": name, "topic": None},
        [call.delete_server_metadata(worker, ["topic"])],
    )
    assert invoke("topic", owner="bob", text="new") == (
        f"{name} belongs to alice, not bob (--force to override)", [],
    )
    result, operations = invoke("claim", owner="bob", force=True, meta=[
        "agent_id=example:one", "agent_id=example:two", "topic=" + "é" * 256,
    ])
    assert result == {"name": name, "agent": None, "agent_id": "example:two",
                      "agent_pid": None, "agent_session": None, "topic": "é" * 255,
                      "cwd": None}
    assert operations == [
        call.set_server_metadata(worker, agent_id="example:two", topic="é" * 255),
        call.delete_server_metadata(worker, ["agent"]),
    ]
    result, operations = invoke("claim", meta=["invalid=one"])
    assert result.endswith("got 'invalid=one'") and not operations
    assert invoke("extend", ttl=60, no_niks3_token=True) == (
        {"name": name, "expires_at": 2200, "max_lifetime_end": 2200},
        [call.set_server_metadata(worker, expires_at="2200")],
    )
    assert invoke("niks3-token") == (
        "niks3 tokens are not configured", [],
    )
    assert invoke("down") == (f"deleted {name}\n", [call.delete(name)])
    if binary is not None:
        worker.metadata.update({"expires_at": "2100", "host_key": "fake"})
        cfg.update({"adhocGracePeriod": 600, "bootTimeout": 120})
        cfg["adhocMaxTtl"] = 3600
        current = json.loads(config.read_text())
        config.write_text(json.dumps({**current, **cfg}))
        cloud = Mock()
        cloud.servers.return_value = {name: worker}
        commands["cmd_reap"](cfg, cloud, None)
        cloud.delete.assert_called_once_with(name)
        cloud.route.assert_not_called()
        trace.write_text("")
        actual = subprocess.run(
            [str(binary), "server", str(config), "reap"], capture_output=True, text=True,
            timeout=10, env={
                **os.environ, "PATH": f"{directory}:{os.environ['PATH']}",
                "RWORKER_FAKE_SERVER": json.dumps({"properties": worker.metadata}),
                "RWORKER_FAKE_TRACE": str(trace),
            },
        )
        assert (actual.returncode, actual.stdout) == (0, ""), actual
        assert actual.stderr == f"deleting {name} (maximum lifetime reached)\n", actual
        assert [json.loads(line) for line in trace.read_text().splitlines()] == [["delete", name]]
        print("Rust vs Python reaper CLI: 1 case compared, 0 differences")
    return 8


if __name__ == "__main__":
    oracle = Path(sys.argv[1]).resolve(strict=True)
    binary = Path(sys.argv[2]).resolve(strict=True) if len(sys.argv) > 2 else None
    with tempfile.TemporaryDirectory(prefix="rworker-metadata-") as directory:
        count = characterize(oracle, binary, Path(directory))
    print(f"Python metadata oracle: {count} cases passed")
    if binary is not None:
        print(f"Rust vs Python metadata: {count} cases compared, 0 differences")
