"""Exercise local control transport using a real Unix socket and profiles."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile


def smoke(binary, directory):
    config = directory / "config.json"
    config.write_text(json.dumps({
        "adhocFlavor": "example.small", "adhocVolumeSize": 16,
        "profiles": {"standard": {}},
    }))
    path = directory / "control.sock"
    env = {**os.environ, "RWORKER_CONTROL_SOCKET": str(path)}
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(path))
        listener.listen()
        listener.settimeout(10)
        env.pop("SSH_CONNECTION", None)
        env.pop("SSH_TTY", None)
        env.pop("RWORKER_CONTROL_SSH_COMPAT", None)
        cases = [
            (["profiles", "--json"], 0, env),
            (["profiles"], 0, {**env, "SSH_CONNECTION": "fake", "RWORKER_CONTROL_SSH_COMPAT": "1"}),
            (["__control-socket", str(path)], 2, env),
        ]
        for args, status, environment in cases:
            with subprocess.Popen(
                [str(binary), *args], env=environment, text=True,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ) as client:
                with listener.accept()[0] as connection:
                    server = subprocess.run(
                        [str(binary), "__serve-control-socket", str(config)],
                        stdin=connection, stdout=connection, stderr=subprocess.PIPE,
                        text=True, timeout=10,
                    )
                output, errors = client.communicate(timeout=10)
                assert server.returncode == 0, server.stderr
                assert client.returncode == status, (output, errors)
                if status == 0:
                    assert not errors, errors
                    assert json.loads(output) == [{
                        "name": "standard", "default": True,
                        "flavor": "example.small", "disk": 16, "image_tags": [],
                    }]
                else:
                    assert not output, output
                    assert "required: command" in errors, errors


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rworker-socket-") as directory:
        smoke(Path(sys.argv[1]).resolve(strict=True), Path(directory))
    print("Unix socket: local and SSH-compatible profile responses and error status passed")
