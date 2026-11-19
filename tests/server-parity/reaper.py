"""Characterize Python reaper decisions without any provider connections."""
import json
import runpy
import subprocess
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from unittest.mock import Mock, patch


def characterize(oracle, driver=None):
    # The only cloud objects the oracle can see are the fakes below.
    with patch.dict(sys.modules, {"openstack": ModuleType("openstack")}):
        reap = runpy.run_path(str(oracle))["cmd_reap"]
    cfg = {
        "adhocPrefix": "example-",
        "adhocMaxTtl": 10000,
        "adhocGracePeriod": 600,
        "bootTimeout": 120,
    }
    now = 4000
    ready = {"created_at": "1000", "expires_at": "3500", "host_key": "fake"}
    cases = [
        ("boot-timeout", {"created_at": "3879"}, None, ["delete"]),
        ("boot-boundary", {"created_at": "3880"}, None, []),
        ("unexpired", {**ready, "expires_at": "4001"}, True, []),
        ("expired-idle", ready, False, ["route", "probe", "delete"]),
        ("max-lifetime", {**ready, "max_lifetime": "3000"}, True, ["delete"]),
        ("unknown-within-grace", ready, None, ["route", "probe"]),
        ("unknown-grace-boundary", {**ready, "expires_at": "3400"}, None,
         ["route", "probe", "delete"]),
        ("idle-after-recent-activity", {**ready, "last_active": "3600"}, False,
         ["route", "probe"]),
        ("busy-refresh", {**ready, "niks3_token_exp": "4200"}, True,
         ["route", "probe", ("last_active", "4000"), ("token", 4600)]),
        ("busy-throttled", {**ready, "last_active": "3700"}, True, ["route", "probe"]),
    ]

    fixtures = []
    for case, meta, active, expected in cases:
        operations = []
        server = SimpleNamespace(metadata=meta)
        cloud = Mock()
        cloud.servers.return_value = {"example-test": server}
        cloud.route.side_effect = lambda name: operations.append("route") or "192.0.2.1"
        cloud.delete.side_effect = lambda name: operations.append("delete")
        cloud.os.compute.set_server_metadata.side_effect = (
            lambda worker, *, last_active: operations.append(("last_active", last_active))
        )
        with patch.dict(reap.__globals__, {
            "time": SimpleNamespace(time=lambda: now),
            "worker_active": lambda *_: operations.append("probe") or active,
            "install_niks3_token": lambda _, cloud, server, end: operations.append(("token", end)),
        }):
            reap(cfg, cloud, None)
        cloud.servers.assert_called_once_with("example-")
        assert operations == expected, (case, operations, expected)
        fixtures.append((case, {
            "config": cfg, "metadata": meta, "now": now, "active": active,
        }, json.loads(json.dumps(operations))))

    if driver is not None:
        result = subprocess.run(
            [str(driver)], input="".join(json.dumps(data) + "\n" for _, data, _ in fixtures),
            capture_output=True, text=True, check=True, timeout=10,
        )
        for (case, _, expected), line in zip(fixtures, result.stdout.splitlines(), strict=True):
            actual = json.loads(line)
            assert actual == expected, (case, actual, expected)
        print(f"Rust vs Python reaper policy: {len(fixtures)} cases compared, 0 differences")
    return len(cases)


if __name__ == "__main__":
    driver = Path(sys.argv[2]).resolve(strict=True) if len(sys.argv) > 2 else None
    count = characterize(Path(sys.argv[1]).resolve(strict=True), driver)
    print(f"Python reaper oracle: {count} cases passed")
