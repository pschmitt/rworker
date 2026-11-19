#!/usr/bin/env python3
"""Differential parity harness: the bash reference client and the Rust
binary over the same corpus of invocations, against the same fake control
host and workers. See ../../PARITY.md.

usage: run.py RUST_BINARY [CASE_SUBSTRING]

Needs bash, jq, column (util-linux), procps and coreutils on PATH for the
reference. Prints every difference and a summary with the denominator.
"""

import datetime
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
REFERENCE = HERE / "reference" / "rworker.sh"
BASE = Path(os.environ.get("PARITY_DIR", Path.home() / "parity-sandbox"))
SANDBOX = BASE / "sandbox"
HOST = socket.gethostname()
ME = f"tester@{HOST}"
NOW = int(time.time())  # reset at the start of every case

# Differences accepted by the contract, by case name: the reason. They
# only arise with a realistic ssh that reads its stdin (the default mode);
# with FAKE_SSH_STDIN=0 every case must match.
STDIN_BUG = "PARITY.md bug 2: bash's control-host ssh swallows a list or script meant for later"
ACCEPTED = {
    "spec": "CHANGELOG: dynamic value sets for the completions, the completions action",
    **{case: "CHANGELOG: the usage lists the completions action" for case in
       ("usage-empty", "help", "help-flag", "help-bogus", "unknown-action")},
    **{case: "CHANGELOG: RWORKER_AWS_CREDENTIALS_FILE in the help" for case in
       ("nix-cache-missing", "nix-cache-bogus")},
    "prune-yes-all-mine": STDIN_BUG,
    "logs-all": STDIN_BUG,
    "logs-follow": STDIN_BUG,
    "ps-long": STDIN_BUG,
    # The workers bash failed to prune are still in its state directory.
    "prune-yes-json": STDIN_BUG,
}
if os.environ.get("FAKE_SSH_STDIN") == "0":
    ACCEPTED = {k: v for k, v in ACCEPTED.items() if v != STDIN_BUG}


def iso(t, frac=False, z=True):
    s = datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S")
    return s + (".123456" if frac else "") + ("Z" if z else "")


def workers():
    # Offsets end on :30 so that a few seconds between the two runs never
    # cross a minute.
    return [
        {"name": "adhoc-aaaa1111", "kind": "adhoc", "status": "ACTIVE", "owner": ME,
         "agent": "claude", "agent_pid": 1234, "agent_id": "claude-code:x", "agent_session": "s-1",
         "topic": "rbw build", "ip": "10.1.0.1", "ips": {"netbird": "10.1.0.1"},
         "host_key": "ssh-ed25519 AAAAone", "expires_at": NOW + 1530, "created_at": iso(NOW - 4170),
         "flavor": "s1.xlarge", "profile": "standard", "connected": True,
         "max_lifetime_end": NOW + 80030, "availability_zone": "az1", "cwd": "/home/tester/src",
         "metadata": {"host_key": "ssh-ed25519 AAAAone", "ttl": "2h", "n": 3}},
        {"name": "adhoc-aaaa2222", "kind": "adhoc", "status": "BUILD", "owner": "other@else",
         "agent": "codex", "topic": "", "ip": "", "host_key": None, "expires_at": NOW + 3030,
         "created_at": iso(NOW - 3150, frac=True, z=False), "flavor": "s1.large"},
        {"name": "adhoc-bbbb3333", "kind": "adhoc", "status": "ERROR", "owner": ME,
         "agent": None, "ip": "192.0.2.1", "ips": {"netbird": "192.0.2.1", "tailscale": "127.0.0.1"},
         "host_key": "ssh-ed25519 AAAAthree", "expires_at": NOW - 90, "grace_until": NOW + 270,
         "last_active": NOW - 630, "created_at": iso(NOW - 10830), "profile": None},
        {"name": "pool-1", "kind": "pool", "status": "ACTIVE", "pool_state": "idle",
         "last_busy": NOW - 750, "owner": "nixbot", "agent": "nixbot", "ip": "10.1.0.4",
         "host_key": "ssh-ed25519 AAAAfour", "created_at": iso(NOW - 93630)},
        {"name": "pool-2", "kind": "pool", "status": "active", "pool_state": "busy",
         "agent": "browser-daemon", "topic": "x\ty", "ip": "10.1.0.5",
         "host_key": "ssh-ed25519 AAAAfive", "created_at": "garbage"},
    ]


# (name, argv, options). Options: tty (pseudo-terminal), env (extra),
# stop (send this signal after N seconds), stdin (text), sleep (seconds
# before the case, for jobs to finish).
CASES = [
    ("usage-empty", [], {}),
    ("help", ["help"], {}),
    ("help-flag", ["--help"], {}),
    ("help-up", ["help", "up"], {}),
    ("help-up-nocolor", ["help", "up"], {"env": {"NO_COLOR": "1"}}),
    ("up-dash-h", ["up", "-h"], {}),
    ("help-bogus", ["help", "bogus"], {}),
    ("unknown-action", ["bogus"], {}),
    ("ls-h", ["ls", "--topic", "x", "-h"], {}),
    ("exec-h-after-value", ["exec", "--ttl", "2h", "-h"], {}),
    ("spec", ["__spec"], {}),
    ("workers-no-cache", ["__workers"], {}),
    ("ls", ["ls"], {}),
    ("ls-json", ["ls", "--json"], {}),
    ("ls-j-anywhere", ["ls", "-o", "wide", "-j"], {}),
    ("ls-wide", ["ls", "-o", "wide"], {}),
    ("ls-wide-noicons", ["ls", "-o", "wide"], {"env": {"RWORKER_ICONS": "0"}}),
    ("ls-o-json", ["ls", "-o", "json"], {}),
    ("ls-o-bogus", ["ls", "-o", "bogus"], {}),
    ("ls-o-missing", ["ls", "-o"], {}),
    ("ls-topic", ["ls", "--topic", "rbw"], {}),
    ("ls-topic-invert", ["ls", "--topic", "!rbw"], {}),
    ("ls-agent", ["ls", "--agent", "^c"], {}),
    ("ls-agent-id", ["ls", "--agent-id", "claude-code"], {}),
    ("ls-topic-invalid", ["ls", "--topic", "("], {}),
    ("ls-topic-missing", ["ls", "--topic"], {}),
    ("ls-interval-zero", ["ls", "-w", "-n", "0"], {}),
    ("ls-unknown", ["ls", "--bogus"], {}),
    ("ls-tty", ["ls"], {"tty": True}),
    ("ls-wide-tty", ["ls", "-o", "wide"], {"tty": True}),
    ("ls-watch", ["ls", "--watch", "-n", "1"], {"stop": (signal.SIGTERM, 2.5), "env": {"TERM": "dumb"}}),
    ("ls-watch-xterm", ["ls", "--watch", "-n", "1"], {"stop": (signal.SIGINT, 2.5)}),
    ("workers", ["__workers"], {}),
    ("workers-extend", ["__workers", "extend"], {}),
    ("inspect", ["inspect", "adhoc-aaaa1111"], {}),
    ("inspect-tty", ["inspect", "adhoc-aaaa1111"], {"tty": True}),
    ("inspect-grace", ["inspect", "adhoc-bbbb3333"], {}),
    ("inspect-pool", ["inspect", "pool-1"], {}),
    ("inspect-pool2", ["inspect", "pool-2"], {}),
    ("inspect-json", ["inspect", "--json", "adhoc-aaaa2222"], {}),
    ("inspect-none", ["inspect", "nope"], {}),
    ("inspect-missing", ["inspect"], {}),
    ("inspect-bogus", ["inspect", "--bogus"], {}),
    ("inspect-boot", ["inspect", "adhoc-aaaa1111", "--boot"], {}),
    ("inspect-boot-json", ["inspect", "adhoc-aaaa1111", "--boot", "-j"], {}),
    ("profiles", ["profiles"], {}),
    ("profiles-json", ["profiles", "--json"], {}),
    ("profiles-tty", ["profiles"], {"tty": True}),
    ("profiles-extra", ["profiles", "x"], {}),
    ("prune-dry", ["prune", "-n"], {}),
    ("prune-dry-json", ["prune", "-n", "--json"], {}),
    ("prune-all-dry", ["prune", "--all", "--dry-run"], {}),
    ("prune-names-dry", ["prune", "adhoc-aaaa2222", "pool-1", "-n"], {}),
    ("prune-nothing", ["prune", "pool-1"], {}),
    ("prune-nothing-json", ["prune", "pool-1", "-j"], {}),
    ("prune-no-tty", ["prune"], {}),
    ("prune-unknown", ["prune", "--bogus"], {}),
    ("down-missing", ["down"], {}),
    ("down", ["down", "adhoc-aaaa2222"], {}),
    ("down-json-force", ["down", "adhoc-aaaa2222", "--force", "--json"], {}),
    ("down-fail", ["down", "fail-1"], {}),
    ("extend", ["extend", "adhoc-aaaa1111", "--ttl", "2h"], {}),
    ("extend-json", ["extend", "adhoc-aaaa1111", "--ttl", "2h", "--json"], {}),
    ("topic", ["topic", "adhoc-aaaa1111", "new", "topic", "--force", "text"], {}),
    ("topic-clear-json", ["topic", "adhoc-aaaa1111", "--json"], {}),
    ("topic-missing", ["topic"], {}),
    ("claim", ["claim", "adhoc-aaaa1111", "--topic", "t", "--agent-id", "id-1"], {}),
    ("claim-no-topic", ["claim", "adhoc-aaaa1111"], {}),
    ("claim-env-id", ["claim", "adhoc-aaaa1111", "--topic", "t", "-j"], {"env": {"CONCORD_AGENT_ID": "c-2"}}),
    ("claim-missing", ["claim"], {}),
    ("claim-two-names", ["claim", "a", "b"], {}),
    ("claim-bogus", ["claim", "a", "--bogus"], {}),
    ("claim-trailing-topic", ["claim", "a", "--topic"], {}),
    ("niks3", ["niks3-token", "adhoc-aaaa1111"], {}),
    ("niks3-fail", ["niks3-token", "fail-2"], {}),
    ("niks3-missing", ["niks3-token"], {}),
    ("builder", ["builder", "adhoc-aaaa1111"], {}),
    ("builder-json", ["builder", "adhoc-aaaa1111", "-j"], {}),
    ("builder-missing", ["builder"], {}),
    ("builder-no-identity", ["builder", "pool-1"], {"env": {"RWORKER_KEY": "/nonexistent.pub"}}),
    ("ssh-echo", ["ssh", "adhoc-aaaa1111", "echo", "hi"], {}),
    ("ssh-user", ["ssh", "-l", "pschmitt", "adhoc-aaaa1111", "true"], {}),
    ("ssh-at-user", ["ssh", "root@adhoc-aaaa1111", "exit", "3"], {}),
    ("ssh-missing", ["ssh"], {}),
    ("ssh-l-missing", ["ssh", "-l"], {}),
    ("ssh-at-only", ["ssh", "x@"], {}),
    ("ssh-adopt-pool", ["ssh", "pool-1", "echo", "pool"], {}),
    ("ssh-not-ready", ["ssh", "adhoc-aaaa2222", "true"], {}),
    ("ssh-unknown", ["ssh", "nope-9", "true"], {}),
    ("ssh-abbrev", ["ssh", "aaaa1", "echo", "abbrev"], {}),
    ("ssh-ambiguous", ["ssh", "adhoc-aaaa", "true"], {}),
    ("ssh-probe", ["ssh", "adhoc-bbbb3333", "echo", "probed"], {"env": {"RWORKER_PROBE_TIMEOUT": "1"}}),
    ("ssh-rworker-user", ["ssh", "adhoc-aaaa1111", "true"], {"env": {"RWORKER_USER": "someone"}}),
    ("sync", ["sync", "adhoc-aaaa1111", "SRC"], {}),
    ("sync-dest-json", ["sync", "pschmitt@adhoc-aaaa1111", "SRC/", "dst", "--json"], {}),
    ("sync-missing", ["sync", "adhoc-aaaa1111"], {}),
    ("sync-nonexistent", ["sync", "adhoc-aaaa1111", "/nonexistent"], {}),
    ("cp-down", ["cp", "adhoc-aaaa1111:foo", "x:y"], {}),
    ("cp-up-json", ["cp", "./SRC", "pschmitt@aaaa1:/tmp/", "--json"], {}),
    ("cp-no-remote", ["cp", "a", "b"], {}),
    ("cp-three", ["cp", "a", "b:c", "d"], {}),
    ("nix-cache-dry", ["nix-cache", "adhoc-aaaa1111", "--dry-run"], {}),
    ("nix-cache-dry-json", ["nix-cache", "-n", "adhoc-aaaa1111", "-j"], {}),
    ("nix-cache-old", ["nix-cache", "adhoc-aaaa1111"], {}),
    ("nix-cache-no-profile", ["nix-cache", "adhoc-aaaa1111"], {"env": {"AWS_SHARED_CREDENTIALS_FILE": "/nonexistent"}}),
    ("nix-cache-missing", ["nix-cache"], {}),
    ("nix-cache-bogus", ["nix-cache", "--bogus"], {}),
    ("jobs-missing-worker", ["jobs"], {}),
    ("start", ["start", "adhoc-aaaa1111", "--name", "j1", "--", "echo hello; echo oops >&2; exit 4"], {}),
    ("start-bad-name", ["start", "adhoc-aaaa1111", "--name", "bad/name", "--", "true"], {}),
    ("jobs", ["jobs", "adhoc-aaaa1111"], {"sleep": 1}),
    ("workers-jobs", ["__jobs", "adhoc-aaaa1111"], {}),
    ("logs-job", ["logs", "adhoc-aaaa1111", "j1"], {}),
    ("logs-worker", ["logs", "--lines", "1", "adhoc-aaaa1111"], {}),
    ("register", ["register", "adhoc-aaaa1111", "ext", "--log", "SRC/file"], {}),
    ("logs-all", ["logs"], {}),
    ("logs-all-lines", ["logs", "--lines", "5", "--topic", "rbw"], {}),
    ("logs-bad-lines", ["logs", "--lines", "x"], {}),
    ("logs-unknown", ["logs", "--bogus"], {}),
    ("logs-topic-empty", ["logs", "--topic="], {}),
    # Concurrent tails on the worker interleave in any order.
    ("logs-follow", ["logs", "-f", "--topic", "rbw", "--no-events"], {"stop": (signal.SIGINT, 3), "unordered": True}),
    ("ps", ["ps", "adhoc-aaaa1111"], {}),
    ("ps-long", ["ps", "--long"], {}),
    ("ps-unknown", ["ps", "nope"], {}),
    ("ps-two", ["ps", "a", "b"], {}),
    ("events-zero", ["events", "--interval", "0"], {}),
    ("events-bogus", ["events", "--bogus"], {}),
    ("events-trailing", ["events", "--interval"], {}),
    ("events", ["events", "--interval", "1", "--topic", "rbw"], {"stop": (signal.SIGTERM, 2.5)}),
    ("up-no-topic", ["up", "--no-nix-cache"], {}),
    ("up-json", ["up", "--topic", "build x", "--json"], {}),
    ("up-tty", ["up", "--topic", "t", "--no-nix-cache"], {"tty": True}),
    ("up-trailing-topic", ["up", "--topic"], {}),
    ("up-l-missing", ["up", "-l"], {}),
    ("up-no-key", ["up", "--topic", "t"], {"env": {"RWORKER_KEY": "/nonexistent.pub"}}),
    ("up-fail", ["up", "--topic", "t", "--profile", "fail"], {}),
    ("ssh-create", ["ssh", "--create", "--topic", "t", "--no-nix-cache"], {}),
    ("exec", ["exec", "--no-nix-cache", "--", "echo", "hi"], {}),
    ("exec-json", ["exec", "--json", "--no-nix-cache", "--", "echo out; echo err >&2; exit 3"], {}),
    ("exec-sync-fetch", ["exec", "--no-nix-cache", "--sync", "SRC:src", "--fetch", "out:./o", "--fetch", "a/b", "--", "true"], {}),
    ("exec-keep-failure", ["exec", "--no-nix-cache", "--keep-on-failure", "--", "false"], {}),
    ("exec-keep", ["exec", "--no-nix-cache", "--keep", "echo", "kept"], {}),
    ("exec-missing", ["exec"], {}),
    ("exec-ttl-missing", ["exec", "--ttl"], {}),
    ("exec-trailing-topic", ["exec", "--topic"], {}),
    ("exec-unknown", ["exec", "--bogus", "--", "true"], {}),
    ("exec-interrupt", ["exec", "--no-nix-cache", "--", "sleep", "3"], {"stop": (signal.SIGTERM, 1.5)}),
    # browser-daemon's rworker provider (nixos-config-private) drives the
    # CLI like this: up, ssh with a secret on stdin, ls, down.
    ("bd-up", ["up", "--json", "--no-nix-cache", "--ttl", "2h", "--profile", "browser-daemon",
               "--topic", "browser-daemon:0b6f0a52-1f43-4c55-9b0e-6a1f3f0a9b1d"], {}),
    ("bd-ssh-secret", ["ssh", "adhoc-aaaa1111", "umask 077; cat > secret.tmp && mv secret.tmp secret && wc -c < secret"],
     {"stdin": "node-secret\n"}),
    ("bd-ls", ["ls", "--json"], {}),
    ("bd-down", ["down", "adhoc-aaaa1111"], {}),
    ("prune-yes-all-mine", ["prune", "-y"], {}),
    ("prune-yes-json", ["prune", "adhoc-aaaa1111", "-y", "--json"], {}),
]

NORMALIZE = [
    (re.compile(r"\r\x1b\[K[^\r\n]*"), ""),  # spinner redraws
    (re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?"), "<ISO>"),
    (re.compile(r"\b\d\d:\d\d:\d\d\b"), "<HMS>"),
    (re.compile(r"\((\x1b\[[0-9;]*m)?\d+s(\x1b\[[0-9;]*m)?\)"), "(<N>s)"),
    (re.compile(r"# Pushed by rworker nix-cache from \S+ at \S+"), "# Pushed <...>"),
    # The fake workers are local child processes, so killing a follower's
    # tree also kills their `sleep`, and bash reports it (racy). Remote
    # processes are no descendants of the client.
    (re.compile(r"(?m)^Terminated +.*\n"), ""),
]


EPOCH = re.compile(r"\b1[0-9]{9}\b")
CLOCK = [
    (re.compile(r"\d{4}-\d\d-\d\d \d\d:\d\d"), "<DATE HM>"),
    (re.compile(r"\((\x1b\[[0-9;]*m)?\d\d:\d\d(\x1b\[[0-9;]*m)?\)"), "(<HM>)"),
    (re.compile(r"\b(Mon|Tue|Wed|Thu|Fri|Sat|Sun) \d\d:\d\d\b"), "<DAY HM>"),
]


def normalize(text):
    for rx, rep in NORMALIZE + CLOCK:
        text = rx.sub(rep, text)
    # Epochs relative to the start of the case.
    text = EPOCH.sub(lambda m: f"<NOW{int(m.group(0)) - NOW:+d}>", text)
    return text.replace(str(SANDBOX), "<SANDBOX>")


def setup(impl_bin):
    if SANDBOX.exists():
        shutil.rmtree(SANDBOX)
    home = SANDBOX / "home"
    fake = SANDBOX / "fake"
    fakebin = SANDBOX / "fakebin"
    for d in (home / ".ssh", home / ".aws", fake, fakebin, SANDBOX / "worker", home / "SRC"):
        d.mkdir(parents=True)
    (home / ".ssh" / "id_ed25519.pub").write_text("ssh-ed25519 AAAAmine tester@example\n")
    (home / ".ssh" / "id_ed25519").write_text("not a key\n")
    (home / ".aws" / "credentials").write_text(
        "[default]\naws_access_key_id = EXAMPLE\n[niks3]\naws_access_key_id = EXAMPLE2\n[other]\nx = 1\n")
    (home / "SRC" / "file").write_text("x\n")
    (fake / "ls.json").write_text(json.dumps(workers()) + "\n")
    for tool in ("ssh", "rsync", "nix"):
        os.symlink(HERE / "fake" / "fake.py", fakebin / tool)
    implbin = SANDBOX / "implbin"
    implbin.mkdir()
    target = implbin / "rworker"
    if impl_bin == "bash":
        # As writeShellApplication runs it.
        target.write_text(f"#!{shutil.which('bash')}\nset -o errexit\nset -o nounset\nset -o pipefail\n"
                          + REFERENCE.read_text())
        target.chmod(0o755)
    else:
        shutil.copy(impl_bin, target)
    return home


def env_for(home, extra):
    env = {
        "PATH": f"{SANDBOX}/fakebin:{SANDBOX}/implbin:{os.environ['PATH']}",
        "HOME": str(home), "USER": "tester", "TZ": "UTC", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
        "RWORKER_HOST": "ctl", "TERM": "xterm-256color",
        "FAKE_CONTROL": "ctl", "FAKE_DIR": str(SANDBOX / "fake"), "FAKE_LOG": str(SANDBOX / "fake" / "log"),
        "FAKE_WORKER_HOME": str(SANDBOX / "worker"), "FAKE_NOW": str(NOW),
        "FAKE_SSH_STDIN": os.environ.get("FAKE_SSH_STDIN", "1"),
    }
    env.update(extra)
    return env


def state_snapshot():
    root = SANDBOX / "home" / ".local" / "state" / "rworker"
    snap = {}
    if root.exists():
        for p in sorted(root.rglob("*")):
            if p.is_file() and not p.name.startswith("control-"):
                text = p.read_text(errors="replace")
                if p.name.endswith(".json"):
                    try:
                        # Same JSON, whatever the key spacing.
                        text = json.dumps(json.loads(text))
                    except ValueError:
                        pass
                # Files written by earlier cases hold their epochs.
                snap[str(p.relative_to(root))] = normalize(EPOCH.sub("<EPOCH>", text))
    return snap


def run_case(argv, opts, home):
    global NOW
    time.sleep(opts.get("sleep", 0))
    # Fixture times are relative to the start of the case, so both runs see
    # the same durations however long the other cases took.
    NOW = int(time.time())
    (SANDBOX / "fake" / "ls.json").write_text(json.dumps(workers()) + "\n")
    argv = [a.replace("SRC", str(home / "SRC")) for a in argv]
    env = env_for(home, opts.get("env", {}))
    log = SANDBOX / "fake" / "log"
    if log.exists():
        log.unlink()
    stop = opts.get("stop")
    cmd = [str(SANDBOX / "implbin" / "rworker"), *argv]
    if opts.get("tty"):
        master, slave = pty.openpty()
        p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=slave, stderr=slave, env=env, cwd=home)
        os.close(slave)
        chunks = []
        while True:
            r, _, _ = select.select([master], [], [], 30)
            if not r:
                break
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            chunks.append(data)
        os.close(master)
        code = p.wait()
        out, err = b"".join(chunks).decode(errors="replace"), ""
    else:
        stdin = opts.get("stdin")
        p = subprocess.Popen(cmd, stdin=subprocess.PIPE if stdin else subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=home)
        if stdin:
            p.stdin.write(stdin.encode())
            p.stdin.close()
        if stop:
            time.sleep(stop[1])
            p.send_signal(stop[0])
        try:
            o, e = p.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            p.kill()
            o, e = p.communicate()
        code = p.returncode
        out, err = o.decode(errors="replace"), e.decode(errors="replace")
    calls = []
    if log.exists():
        for line in log.read_text().splitlines():
            calls.append(normalize(line))
    if opts.get("unordered"):
        out, err = ("\n".join(sorted(t.splitlines())) for t in (out, err))
    return {"code": code, "stdout": normalize(out), "stderr": normalize(err),
            "calls": calls, "state": state_snapshot()}


def run_all(impl, selected):
    home = setup(impl)
    results = {}
    for name, argv, opts in selected:
        results[name] = run_case(argv, opts, home)
    # Leftover jobs of this run must not leak into the next one.
    subprocess.run(["pkill", "-f", f"{SANDBOX}/worker"], check=False)
    return results


def main():
    rust = sys.argv[1]
    only = sys.argv[2] if len(sys.argv) > 2 else ""
    selected = [c for c in CASES if only in c[0]]
    bash = run_all("bash", selected)
    ours = run_all(rust, selected)
    differences = accepted = 0
    for name, _, _ in selected:
        a, b = bash[name], ours[name]
        diffs = [k for k in ("code", "stdout", "stderr", "calls", "state") if a[k] != b[k]]
        if not diffs:
            continue
        if name in ACCEPTED:
            accepted += 1
            print(f"ACCEPTED {name}: {ACCEPTED[name]} ({', '.join(diffs)})")
            continue
        differences += 1
        print(f"=== DIFF {name}: {', '.join(diffs)}")
        for k in diffs:
            print(f"--- {k} (bash)\n{json.dumps(a[k], indent=1) if k in ('calls', 'state') else repr(a[k])}")
            print(f"+++ {k} (rust)\n{json.dumps(b[k], indent=1) if k in ('calls', 'state') else repr(b[k])}")
    print(f"\ncompared {len(selected)} invocations: {differences} differences, {accepted} accepted")
    sys.exit(1 if differences else 0)


if __name__ == "__main__":
    main()
