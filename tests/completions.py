#!/usr/bin/env python3
"""Exercise the installed Bash and Zsh completions through real line editors."""

import os
import pty
import select
import shlex
import signal
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CASES = [
    ("rworker ls top", "rworker ls topic:"),
    ("rworker ls topic:cir", r"rworker ls topic:circus\ build\ pool "),
    ("rworker nodes top", "rworker nodes topic:"),
    ("rworker ls --topic cir", r"rworker ls --topic circus\ build\ pool "),
    ("rworker logs --filter topic:ci", "rworker logs --filter topic:ci"),
    ("rworker logs -f agent:co", "rworker logs -f agent:codex "),
    ("rworker logs -f agent-id:codex:a", "rworker logs -f agent-id:codex:abc "),
    ("rworker logs -f topic:cir agent:co", "rworker logs -f topic:cir agent:codex "),
    ("rworker logs -f topic:nix adhoc-t", "rworker logs -f topic:nix adhoc-test "),
    ("rworker logs -f topic:nix adhoc-test my", "rworker logs -f topic:nix adhoc-test myjob "),
]


def drain(fd):
    output = b""
    while select.select([fd], [], [], 0)[0]:
        try:
            output += os.read(fd, 32768)
        except OSError:
            break
    return output


def run_shell(shell, capture):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(TERM="xterm", NO_COLOR="1", LC_ALL="C.UTF-8")
        argv = [shell, "-dfi"] if shell == "zsh" else [shell, "--noprofile", "--norc", "-i"]
        os.execvp(shell, argv)
    try:
        spec = shlex.quote(str(REPO / "src/text/spec.txt"))
        completion = shlex.quote(str(REPO / ("src/completions/_rworker" if shell == "zsh" else "src/completions/rworker.bash")))
        output = shlex.quote(str(capture))
        mock = (
            "rworker() { case \"$1\" in "
            f"__spec) cat {spec} ;; "
            "__action) case \"$2\" in ls|machines|nodes|instances) echo machines ;; logs) echo logs ;; esac ;; "
            "__values) case \"$2\" in topic-re) printf 'circus build pool\\tworker\\n' ;; "
            "agent-re) printf 'codex\\tworker\\n' ;; id-re) printf 'codex:abc\\tworker\\n' ;; esac ;; "
            "__workers) printf 'adhoc-test\\tworker\\n' ;; __jobs) printf 'myjob\\n' ;; esac; }\n"
        )
        if shell == "zsh":
            setup = (
                "autoload -Uz compinit; compinit -D\n" + mock
                + f"source {completion} 2>/dev/null\ncompdef _rworker rworker\n"
                + f"__capture() {{ print -rn -- \"$BUFFER\" > {output}; }}\n"
                + "zle -N __capture\nbindkey '^X^P' __capture\n"
            )
        else:
            setup = (
                mock + f"source {completion}\n"
                + f"__capture() {{ printf '%s' \"$READLINE_LINE\" > {output}; }}\n"
                + "bind -x '\"\\C-x\\C-p\":__capture'\n"
            )
        marker = b"\r\nRWORKER_COMPLETION_READY\r\n"
        os.write(fd, (setup + "printf '\\n%s\\n' RWORKER_COMPLETION_READY\n").encode())
        ready = b""
        deadline = time.monotonic() + 15
        while marker not in ready and time.monotonic() < deadline:
            if select.select([fd], [], [], 0.2)[0]:
                ready += os.read(fd, 32768)
        if marker not in ready:
            raise AssertionError(f"{shell}: shell did not become ready")
        quote_case = ('rworker ls topic:"cir', 'rworker ls topic:"circus build pool" ' if shell == "bash" else r"rworker ls topic:circus\ build\ pool ")
        negative_case = ('rworker ls topic:!cir', r"rworker ls topic:\!circus\ build\ pool " if shell == "bash" else r"rworker ls topic:!circus\ build\ pool ")
        cases = CASES + [quote_case, negative_case]
        for typed, expected in cases:
            capture.unlink(missing_ok=True)
            os.write(fd, b"\x15" + typed.encode() + b"\t")
            time.sleep(0.4)
            os.write(fd, b"\x18\x10")
            deadline = time.monotonic() + 5
            transcript = b""
            while (not capture.exists() or capture.stat().st_size == 0) and time.monotonic() < deadline:
                transcript += drain(fd)
                time.sleep(0.05)
            actual = capture.read_text() if capture.exists() else "<no capture>"
            if actual != expected:
                raise AssertionError(f"{shell}: {typed!r}: expected {expected!r}, got {actual!r}; terminal: {transcript[-2000:]!r}")
        print(f"{shell}: {len(cases)} completion cases passed", flush=True)
    finally:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.close(fd)
        os.waitpid(pid, 0)


def main():
    shells = sys.argv[1:] or ["bash", "zsh"]
    with tempfile.TemporaryDirectory(prefix="rworker-completions-") as directory:
        for shell in shells:
            run_shell(shell, Path(directory) / "buffer")


if __name__ == "__main__":
    main()
