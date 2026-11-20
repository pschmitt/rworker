"""Compare a single pool reconciliation with fake cloud, database and SSH."""
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import time
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock, patch


def compare(oracle, binary, root):
    with patch.dict(sys.modules, {"openstack": ModuleType("openstack")}):
        autoscaler = runpy.run_path(str(oracle))["Autoscaler"]
    settings = {
        "flavor": "small", "volumeSize": 16, "netbirdWorkersGroup": "nixbot",
        "networkId": "network", "securityGroup": "security",
        "profiles": {"standard": {"imageTags": ["worker"]}},
        "pollSeconds": 60, "namePrefix": "pool-", "minWorkers": 0, "maxWorkers": 2,
        "bootTimeout": 120, "scaleUpPending": 0, "scaleUpAfter": 60, "idleSeconds": 60, "localMaxJobs": 1,
        "maxJobs": 8, "supportedFeatures": ["big-parallel"], "sshUser": "builder",
        "sshKeyFile": "/run/test/ssh-key", "machinesSshKeyFile": "/run/test/ssh-key",
        "databaseUrl": "postgresql:///test?host=/run/fixture", "netbirdSelf": "fixture",
        "netbirdNixbotGroup": "nixbot", "netbirdApi": "https://overlay.example.invalid",
        "netbirdTokenFile": str(root / "token"), "openstackSecretName": "test",
        "openstackSecretFile": str(root / "secret"), "staticMachinesFile": str(root / "static"),
    }
    (root / "static").write_text("ssh://static x86_64-linux - 4\n")
    (root / "token").write_text("fake")
    (root / "secret").write_text(json.dumps({"test": {"data": dict.fromkeys(
        ["auth_url", "region", "application_credential_id", "application_credential_secret"], "fake"
    )}}))
    for name, script in {
        "openstack": """
import json, os, sys
match sys.argv[1:3]:
    case ["server", "list"]: print(os.environ["FAKE_SERVERS"])
    case ["server", "show"]: print(os.environ["FAKE_DETAIL"])
    case ["server", "set"]: pass
    case ["flavor", "show"]: print('{"id":"flavor","vcpus":1,"ram":512}')
    case ["limits", "show"]:
        print(json.dumps([{"Name": key, "Value": value} for key, value in dict(
            max_total_cores=10, total_cores_used=0, max_total_ram_size=4096,
            total_ram_used=0, max_total_instances=10, total_instances_used=0,
        ).items()]))
    case ["image", "list"]: print('[{"ID":"image"}]')
    case ["image", "show"]: print('{"name":"image","created_at":"2026-01-01"}')
    case ["server", "create"]:
        assert sys.argv[-1].startswith("pool-")
        assert "nixbot-worker=pool" in sys.argv
        print('{"id":"created"}')
    case other: raise AssertionError(other)
""",
        "curl": """
import sys
request = sys.stdin.read()
assert "overlay.example.invalid" in request
if "/setup-keys" in request:
    print('{"id":"key","key":"fake-key"}')
else:
    print('[{"name":"nixbot","id":"group"}]' if "/groups" in request else "[]")
""",
        "psql": """
import os
assert os.environ["PGDATABASE"] == "test"
assert os.environ["PGHOST"] == "/run/fixture"
print("0|0")
""",
        "timeout": "pass",
    }.items():
        exe = root / name
        exe.write_text(f"#!{sys.executable}\n{script}\n")
        exe.chmod(0o700)

    def config(side, workers):
        base = root / side
        base.mkdir(exist_ok=True)
        (base / "state.json").write_text(json.dumps(workers))
        cfg = {**settings, "stateDir": str(base), "machinesFile": str(base / "machines")}
        (base / "config").write_text(json.dumps(cfg))
        return cfg, base

    for case in ["empty", "drain", "recover", "scale-up"]:
        ready = case == "drain"
        recover = case == "recover"
        present = ready or recover
        scale_up = case == "scale-up"
        settings["minWorkers"] = int(recover or scale_up)
        now = int(time.time())
        worker = {"id": "test", "ready": True, "hostKey": "fake-key", "ip": "127.0.0.1",
                  "lastBusy": now - 180, "routeCheck": now}
        workers = {"pool-test": worker} if ready else {}
        meta = {"host_key": "fake-key", "owner": "nixbot@fixture", "nixbot-worker": "pool",
                "agent": "nixbot", "topic": "build pool"}
        py_cfg, py_root = config("python", workers)
        cloud = Mock()
        cloud.servers.return_value = {
            "pool-test": SimpleNamespace(id="test", metadata=meta)
        } if present else {}
        cloud.boot_status.return_value = "booting"
        cloud.create.return_value = (SimpleNamespace(id="created"), "key")
        py = autoscaler(py_cfg, cloud)
        py.demand = lambda: (0, 0)
        py.busy = lambda _: False
        py.write_machines()
        py.reconcile()
        if scale_up:
            cloud.create.assert_called_once()
        else:
            cloud.create.assert_not_called()

        _, rust_root = config("rust", workers)
        result = subprocess.run(
            [str(binary), "server", str(rust_root / "config"), "autoscale", "--once"],
            capture_output=True, text=True, timeout=10, env={
                **os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                "FAKE_SERVERS": json.dumps([{"ID": "test", "Name": "pool-test"}] if present else []),
                "FAKE_DETAIL": json.dumps({"properties": meta, "status": "BUILD"}),
            },
        )
        assert result.returncode == 0, result
        for file in ["machines", "known_hosts"]:
            assert (rust_root / file).read_text() == (py_root / file).read_text(), file
        before = json.loads((py_root / "state.json").read_text())
        after = json.loads((rust_root / "state.json").read_text())
        if ready:
            assert abs(before["pool-test"].pop("drainQuietSince") - after["pool-test"].pop("drainQuietSince")) < 10
        if recover:
            assert after["pool-test"]["recovering"] is True
            assert abs(before["pool-test"].pop("created") - after["pool-test"].pop("created")) < 10
        if scale_up:
            (py_name, py_worker), = before.items()
            (rust_name, rust_worker), = after.items()
            assert py_name.startswith("pool-") and rust_name.startswith("pool-")
            assert abs(py_worker.pop("created") - rust_worker.pop("created")) < 10
            assert py_worker == rust_worker
            continue
        assert before == after, (before, after)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-autoscale-") as directory:
        compare(*(Path(arg).resolve(strict=True) for arg in sys.argv[1:]), Path(directory))
    print("Rust vs Python autoscaler: 4 cases compared, 0 differences")
