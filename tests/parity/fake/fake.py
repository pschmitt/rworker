#!/usr/bin/env python3
"""Fake ssh, rsync and nix for the parity harness (selected by argv[0]).

ssh to the control host (FAKE_CONTROL) answers `rworker ...` from canned
data; ssh to a worker runs the command locally in FAKE_WORKER_HOME, so the
real job manager payload executes. Every call is appended to FAKE_LOG as
one JSON line, with the remote command as the words a shell would see.
"""

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

# ssh options that take an argument.
SSH_ARG_OPTS = set("BbcDEeFIiJLlmOoPpRSWw")


def log(entry):
    with open(os.environ["FAKE_LOG"], "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, sort_keys=True) + "\n")


def read_stdin():
    if sys.stdin is None or sys.stdin.isatty():
        return b""
    try:
        return sys.stdin.buffer.read()
    except OSError:
        return b""


def counter(name):
    path = Path(os.environ["FAKE_DIR"]) / f"counter-{name}"
    n = int(path.read_text()) + 1 if path.exists() else 1
    path.write_text(str(n))
    return n


def meta_of(args):
    meta = {}
    for i, a in enumerate(args):
        if a == "--meta" and i + 1 < len(args):
            k, _, v = args[i + 1].partition("=")
            meta[k] = v
    return meta


def control(args):
    now = int(os.environ["FAKE_NOW"])
    sub = args[0] if args else ""
    if sub == "ls":
        sys.stdout.write(Path(os.environ["FAKE_DIR"], "ls.json").read_text())
        return 0
    if sub == "profiles":
        print(json.dumps([
            {"name": "large-builder", "flavor": "s1.2xlarge", "disk": 500, "image_tags": ["rworker"], "default": False},
            {"name": "standard", "flavor": "s1.xlarge", "disk": 150, "image_tags": ["rworker", "x86"], "default": True},
            {"name": "browser-daemon", "flavor": "s1.large", "disk": 100, "image_tags": ["browser"], "default": False},
        ]))
        return 0
    if sub == "up":
        if "fail" in args:
            print("quota exceeded", file=sys.stderr)
            return 1
        n = counter("up")
        name = f"adhoc-new{n}"
        for line in ("rworker-progress BUILD", f"creating {name} (s1.xlarge) from image rworker-abc",
                     "rworker-progress ACTIVE · booting", "server is up"):
            print(line, file=sys.stderr, flush=True)
            time.sleep(0.05)
        print(json.dumps({
            "name": name, "ip": f"10.9.0.{n}", "ips": {"netbird": f"10.9.0.{n}"},
            "host_key": f"ssh-ed25519 AAAAnew{n}", "user": "agent", "expires_at": now + 7230,
            "flavor": "s1.xlarge", "profile": "standard", "builder": f"ssh-ng://agent@10.9.0.{n}",
            "niks3_token": {"ok": n % 2 == 1, "error": "token service down"},
        }))
        return 0
    name = args[1] if len(args) > 1 else ""
    if sub == "down":
        if name.startswith("fail"):
            print(f"refusing to delete {name}", file=sys.stderr)
            return 1
        print(json.dumps({"name": name, "deleted": True}))
        return 0
    if sub == "extend":
        print(json.dumps({"name": name, "expires_at": now + 7230, "max_lifetime_end": now + 80000,
                          "niks3_token": {"ok": False, "error": "boom"}}))
        return 0
    if sub == "topic":
        print(json.dumps({"name": name, "topic": args[2] if len(args) > 2 and args[2] else None}))
        return 0
    if sub == "claim":
        m = meta_of(args)
        print(json.dumps({"name": name, "agent": m.get("agent"), "agent_id": m.get("agent_id"),
                          "topic": m.get("topic")}))
        return 0
    if sub == "niks3-token":
        if name.startswith("fail"):
            print(json.dumps({"name": name, "niks3_token": {"ok": False, "error": "image-too-old"}}))
        else:
            print(json.dumps({"name": name, "niks3_token": {"ok": True, "expires_at": now + 3630}}))
        return 0
    print(f"unknown rworker action {sub}", file=sys.stderr)
    return 2


def worker(command, stdin):
    home = os.environ["FAKE_WORKER_HOME"]
    if command.startswith("cat /run/rworker/metadata.json"):
        print(json.dumps({"image": "rworker-abc", "created_by": {"agent": "shell"}}))
        return 0
    if command == "test -d /run/rworker-nix":
        return 1
    env = {k: v for k, v in os.environ.items() if not k.startswith("FAKE_")}
    env["HOME"] = home
    # Its own session: a remote process is no descendant of the client, so
    # the client's process-tree kill must not reach it.
    p = subprocess.run(["bash", "-c", command], input=stdin, cwd=home, env=env,
                       start_new_session=True)
    return p.returncode


def ssh(argv):
    opts, i = [], 0
    while i < len(argv):
        a = argv[i]
        if a.startswith("-") and len(a) > 1:
            if a[1] in SSH_ARG_OPTS and len(a) == 2:
                opts += [a, argv[i + 1] if i + 1 < len(argv) else ""]
                i += 2
                continue
            opts.append(a)
            i += 1
            continue
        break
    dest = argv[i] if i < len(argv) else ""
    command = " ".join(argv[i + 1:])
    # Real ssh reads its stdin, also for the control host (which ignores
    # it). FAKE_SSH_STDIN=0 models an ssh that leaves it alone there.
    control_host = dest == os.environ["FAKE_CONTROL"]
    stdin = b"" if control_host and os.environ.get("FAKE_SSH_STDIN") == "0" else read_stdin()
    try:
        words = shlex.split(command)
    except ValueError:
        words = [command]
    log({"tool": "ssh", "opts": opts, "dest": dest, "words": words,
         "stdin_lines": len(stdin.splitlines())})
    if control_host:
        if not words or words[0] != "rworker":
            return 2
        return control(words[1:])
    if not command:
        return 0
    return worker(command, stdin)


def rsync(argv):
    log({"tool": "rsync", "args": argv})
    return 0


def nix(argv):
    log({"tool": "nix", "args": argv})
    if argv[:3] == ["config", "show", "--json"]:
        print(json.dumps({
            "substituters": {"value": ["https://cache.nixos.org/", "s3://cache-b?profile=niks3&region=x"]},
            "extra-substituters": {"value": ["s3://cache-a?region=y"]},
            "trusted-public-keys": {"value": ["cache.nixos.org-1:EXAMPLE=", "b-1:EXAMPLE="]},
        }))
        return 0
    return 1


def main():
    tool = Path(sys.argv[0]).name
    handler = {"ssh": ssh, "rsync": rsync, "nix": nix}[tool]
    sys.exit(handler(sys.argv[1:]))


if __name__ == "__main__":
    main()
