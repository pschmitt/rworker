"""Run the initial control-host contract cases against the Python oracle."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def characterize(oracle, root, binary=None):
    # These cases must finish before constructing any provider client.
    (root / "openstack.py").write_text(
        "def connect(**kwargs):\n"
        "    raise AssertionError('unexpected provider access')\n"
    )
    env = {**os.environ, "PYTHONPATH": str(root), "PYTHONDONTWRITEBYTECODE": "1"}
    config = root / "config.json"
    config.write_text(json.dumps({
        "adhocFlavor": "example.small",
        "adhocVolumeSize": 16,
        "profiles": {
            "standard": {},
            "large": {"flavor": "example.large", "disk": 64, "imageTags": ["test"]},
        },
    }))

    def run(*args):
        return subprocess.run(
            [sys.executable, str(oracle), str(config), *args],
            capture_output=True, text=True, env=env, timeout=10,
        )

    def run_rust(*args):
        return subprocess.run(
            [str(binary), "server", str(config), *args],
            capture_output=True, text=True, env=env, timeout=10,
        )

    def compare_profiles(reference):
        if binary is None:
            return
        actual = run_rust("profiles")
        assert (actual.returncode, actual.stderr) == (reference.returncode, reference.stderr), actual
        assert json.loads(actual.stdout) == json.loads(reference.stdout), actual

    result = run("profiles")
    assert (result.returncode, result.stderr) == (0, ""), result
    assert json.loads(result.stdout) == [
        {"name": "large", "default": False, "flavor": "example.large",
         "disk": 64, "image_tags": ["test"]},
        {"name": "standard", "default": True, "flavor": "example.small",
         "disk": 16, "image_tags": []},
    ]

    compare_profiles(result)

    config.write_text("{}")
    result = run("profiles")
    assert (result.returncode, result.stderr, result.stdout) == (0, "", "[]\n"), result

    compare_profiles(result)

    errors = [
        ((), "the following arguments are required: command"),
        (("extend", "example",), "the following arguments are required: --ttl"),
        (("up", "--ttl", "1d"), "invalid duration: 1d"),
        (("up", "--ttl", "1m1h"), "invalid duration: 1m1h"),
        (("up", "--disk", "1.5"), "invalid int value: '1.5'"),
    ]
    for args, diagnostic in errors:
        result = run(*args)
        assert (result.returncode, result.stdout) == (2, ""), result
        assert diagnostic in result.stderr, result
        if binary is not None:
            actual = run_rust(*args)
            assert (actual.returncode, actual.stdout) == (result.returncode, result.stdout), actual
            assert diagnostic in actual.stderr, actual
    return len(errors) + 2


if __name__ == "__main__":
    oracle = Path(sys.argv[1]).resolve(strict=True)
    binary = Path(sys.argv[2]).resolve(strict=True) if len(sys.argv) > 2 else None
    with tempfile.TemporaryDirectory(prefix="rworker-oracle-") as directory:
        count = characterize(oracle, Path(directory), binary)
    print(f"Python oracle: {count} cases passed")
    if binary is not None:
        print("Rust vs Python: 7 cases compared, 0 differences")
