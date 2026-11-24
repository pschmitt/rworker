"""MCP server for ad-hoc build workers (rworker).

A thin layer over the `rworker` client (pkgs/rworker), so host-key
pinning, local state and SSH keys work exactly as on the command line:
create a short-lived encrypted build VM, run commands or long jobs on it,
copy sources over, use it as a Nix builder, and delete it again.
"""

import argparse
import json
import os
import signal
import subprocess
import time

from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError

ap = argparse.ArgumentParser()
ap.add_argument("--rworker", required=True, help="path to the rworker client")
args = ap.parse_args()

mcp = MCPServer("rworker")

# Keep tool results small: build logs can be megabytes.
MAX_OUTPUT = 20_000


def tail(text, limit=MAX_OUTPUT):
    if len(text) <= limit:
        return text, False
    return text[-limit:], True


def run(*argv, timeout=900):
    """Run the rworker client; raise with its stderr on failure."""
    try:
        proc = subprocess.run(
            [args.rworker, *argv],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        raise ToolError(f"rworker {argv[0]} timed out after {timeout} seconds") from error
    except OSError as error:
        raise ToolError(f"Could not start rworker: {error}") from error
    if proc.returncode != 0:
        raise command_error(argv[0], proc.stdout, proc.stderr)
    return proc.stdout


def command_error(action, stdout, stderr):
    detail, _ = tail(stderr.strip() or stdout.strip(), 4000)
    return ToolError(f"rworker {action} failed: {detail}")


def run_bounded(*argv, seconds):
    """Stop a timed command and its child processes, allowing CLI cleanup."""
    with subprocess.Popen(
        [args.rworker, *argv],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    ) as proc:
        try:
            stdout, stderr = proc.communicate(timeout=seconds)
            expired = False
        except subprocess.TimeoutExpired:
            expired = True
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                stdout, stderr = proc.communicate(timeout=30)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                stdout, stderr = proc.communicate()
        return proc.returncode, stdout, stderr, expired


def observe(argv, seconds, limit):
    if not 1 <= seconds <= 120 or not 1 <= limit <= 1000:
        raise ToolError("Watch requires 1–120 seconds and a limit of 1–1000")
    code, stdout, stderr, expired = run_bounded(*argv, seconds=seconds)
    if code and not expired:
        raise command_error(argv[0], stdout, stderr)
    # JSON watch modes emit complete snapshots or events, one per line.
    lines = stdout.splitlines()
    if expired and stdout and not stdout.endswith("\n"):
        lines.pop()
    return [json.loads(line) for line in lines[-limit:] if line.strip()]


def provision_flags(ttl, flavor, disk_gib, scratch, topic, agent_id,
                    nix_cache, nix_auth, profile, niks3_token, arch, queue_timeout):
    argv = ["--ttl", ttl]
    for flag, value in (
        ("--flavor", flavor), ("--disk", disk_gib), ("--topic", topic),
        ("--agent-id", agent_id), ("--profile", profile), ("--arch", arch),
        ("--queue-timeout", queue_timeout),
    ):
        if value is not None:
            argv += [flag, str(value)]
    if scratch:
        argv.append("--scratch")
    if not nix_cache:
        argv.append("--no-nix-cache")
    if not nix_auth:
        argv.append("--no-nix-auth")
    if not niks3_token:
        argv.append("--no-niks3-token")
    return argv


@mcp.tool()
def up(
    ttl: str = "2h",
    flavor: str | None = None,
    disk_gib: int | None = None,
    scratch: bool = False,
    topic: str | None = None,
    agent_id: str | None = None,
    nix_cache: bool = True,
    nix_auth: bool = True,
    profile: str | None = None,
    niks3_token: bool = True,
    arch: str | None = None,
    queue_timeout: int | None = None,
) -> dict:
    """Create an ad-hoc build worker and wait until it is ready (2-4 min).

    ttl: lifetime such as "90m" or "3h" (default 2h). A worker still in use
    when it expires stays until it has been idle for 1 h, up to its maximum
    lifetime (24 h, or 7 days with scratch=True). topic: ALWAYS set it - a few words on what
    it is for (shown by ls/inspect; the creating agent, its PID, session and
    cwd are recorded automatically). agent_id: ALWAYS set it to your Concord
    agent id, so workers can be filtered by it. flavor: s1.large (8 vCPU), s1.xlarge (16, default),
    s1.2xlarge (32). profile selects a configured worker profile: standard,
    browser-daemon, large-builder or a hosted CI profile shown by profiles().
    arch selects matching profiles instead. queue_timeout cancels a hosted
    lease still queued after that many seconds. Profile defaults can be overridden with
    flavor and disk_gib. disk_gib: volume size (default 150). nix_cache (default
    True) lets the worker substitute from the private S3 Nix cache with this
    host's read-only credentials (pushed over SSH, only if this host has an
    s3:// substituter); pass False to keep the key off the worker. The result
    then has a nix_cache object, ok=false if it did not work. nix_auth
    (default True) also pushes this host's Nix access-tokens (only if it has
    any), so the worker fetches private flake inputs itself; pass False to
    keep the tokens off the worker. Both results are in the nix_cache object
    (auth, auth_hosts).
    niks3_token (default True) gives the worker its own short-lived niks3
    push token, valid as long as the worker (extend() renews it), which the
    `niks3` command on the worker uses (`niks3 push PATH`); the result has a
    niks3_token object, ok=false if it did not work. Pass False to keep it
    off the worker. The browser-daemon profile never gets one.
    Returns name, ip, host_key, user and expires_at. Always call down() when
    the task is done.
    """
    flags = provision_flags(ttl, flavor, disk_gib, scratch, topic, agent_id,
                            nix_cache, nix_auth, profile, niks3_token, arch, queue_timeout)
    return json.loads(run("up", "--json", *flags, timeout=1200))


@mcp.tool()
def profiles(name: str | None = None, wide: bool = False) -> list:
    """List profiles and resources, optionally selecting a name or adding provider and image details."""
    argv = ["profiles", "--json"]
    if name is not None:
        argv.append(name)
    if wide:
        argv.append("--wide")
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def ls(topic: str | None = None, agent: str | None = None,
       agent_id: str | None = None, include_retired: bool = False,
       fresh: bool = False, watch_seconds: int | None = None,
       interval_seconds: int | None = None, limit: int = 10) -> list:
    """List all workers: ad-hoc ones (kind adhoc) and nixbot's pool (pool).
    topic, agent (claude, codex, ...) and agent_id (a Concord agent id) are
    regexes that keep only matching workers; a leading "!" inverts one.
    include_retired adds retained worker history. fresh refreshes inventory.
    Set watch_seconds to return up to limit inventory snapshots over that period.
    """
    argv = ["ls", "--json"]
    if include_retired:
        argv.append("--all")
    if fresh:
        argv.append("--fresh")
    for flag, value in (("--topic", topic), ("--agent", agent),
                        ("--agent-id", agent_id)):
        if value:
            argv += [flag, value]
    if watch_seconds is not None:
        interval = 5 if interval_seconds is None else interval_seconds
        if not 1 <= watch_seconds <= 120 or interval < 1 or not 1 <= limit <= 1000:
            raise ToolError("Invalid inventory watch duration, interval or limit")
        if fresh:
            raise ToolError("fresh cannot be combined with watch_seconds")
        deadline = time.monotonic() + watch_seconds
        snapshots = []
        while (remaining := deadline - time.monotonic()) > 0 and len(snapshots) < limit:
            code, stdout, stderr, expired = run_bounded(*argv, seconds=remaining)
            if expired:
                break
            if code:
                raise command_error(argv[0], stdout, stderr)
            snapshots.append(json.loads(stdout))
            if len(snapshots) < limit:
                time.sleep(min(interval, max(0, deadline - time.monotonic())))
        return snapshots
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def inspect(name: str, boot: bool = False) -> dict:
    """Everything known about one worker: owner, creator (agent, agent id,
    PID, session, topic, cwd), expiry, grace period, max lifetime and server
    metadata. boot=True adds the worker's own /run/rworker/metadata.json."""
    argv = ["inspect", name, "--json"]
    if boot:
        argv.append("--boot")
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def extend(name: str, ttl: str, niks3_token: bool = True, force: bool = False) -> dict:
    """Move a worker's expiry to now + ttl (capped at its maximum lifetime).
    A worker that has a niks3 push token gets a new one for the new expiry
    (the result's niks3_token object); niks3_token=False skips that.
    Set force=True to extend another owner's worker."""
    argv = ["extend", name, "--ttl", ttl, "--json"]
    if not niks3_token:
        argv.append("--no-niks3-token")
    if force:
        argv.append("--force")
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def claim(name: str, topic: str, agent_id: str, force: bool = False) -> dict:
    """Take over an ad-hoc worker you reuse for other work: replaces its
    creator info (agent, agent id, PID, session, cwd) with yours and sets the
    topic. Call it whenever you reuse a worker, with your Concord agent id and
    a short topic ("rbw release build"). Set force=True for another owner's worker."""
    argv = ["claim", name, "--topic", topic, "--agent-id", agent_id, "--json"]
    if force:
        argv.append("--force")
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def topic(name: str, text: str, force: bool = False) -> dict:
    """Change what an ad-hoc worker is for (the topic `ls` shows). Call it
    when you reuse a worker for different work, so `ls` stays truthful for
    everyone sharing the pool; keep it short ("rbw release build"). An empty
    text clears it. Set force=True for another owner's worker. When reusing
    a worker, claim() also takes over agent and agent id."""
    argv = ["topic", name, text, "--json"]
    if force:
        argv.append("--force")
    return json.loads(run(*argv, timeout=120))


@mcp.tool()
def setup_niks3_token(name: str, force: bool = False) -> dict:
    """(Re)issue a worker's short-lived niks3 push token for the rest of its
    lifetime: for a worker created with niks3_token=False, or to retry a
    failed install. The `niks3` command on the worker then pushes to the
    cache (`niks3 push PATH`, pins limited to rworker-*). Set force to
    reissue a token for someone else's worker."""
    argv = ["setup", "niks3-token", name, "--json"]
    if force:
        argv.append("--force")
    return json.loads(run(*argv, timeout=180))


@mcp.tool()
def setup_netbird(name: str) -> dict:
    """Start or restart an opted-in CI worker's ephemeral NetBird proxy."""
    return json.loads(run("setup", "netbird", name, "--json", timeout=330))


@mcp.tool()
def setup_tailscale(name: str) -> dict:
    """Start or restart an opted-in CI worker's ephemeral Tailscale proxy."""
    return json.loads(run("setup", "tailscale", name, "--json", timeout=330))


@mcp.tool()
def down(name: str, force: bool = False) -> dict:
    """Delete a worker and its resources. Force is required for other owners or pool workers."""
    argv = ["down", name, "--json"]
    if force:
        argv.append("--force")
    return json.loads(run(*argv, timeout=300))


@mcp.tool()
def prune(names: list[str] | None = None, all: bool = False,
          dry_run: bool = False) -> list:
    """Delete several ad-hoc workers: the named ones, or else all of this
    user's; all=True deletes every ad-hoc worker, whoever owns it. Pool
    workers are never pruned. dry_run=True only lists them."""
    argv = ["prune", "--json", *(names or [])]
    if all:
        argv.append("--all")
    argv.append("--dry-run" if dry_run else "--yes")
    return json.loads(run(*argv, timeout=900))


@mcp.tool()
def exec(name: str, command: str, workdir: str | None = None,
         timeout_seconds: int = 600) -> dict:
    """Run a registered job and wait. On timeout the job continues; use logs()."""
    job = start(name, command, workdir=workdir)
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        listing = run("jobs", name, timeout=60)
        for row in listing.splitlines():
            fields = row.split("\t")
            if len(fields) >= 2 and fields[0] == job and fields[1].startswith("exit "):
                output, truncated = tail(run("logs", name, job, "--lines", "200", timeout=60))
                return {"job": job, "exit_code": int(fields[1][5:]),
                        "stdout": output, "stderr": "", "truncated": truncated}
        time.sleep(min(2, max(0, deadline - time.monotonic())))
    return {"job": job, "exit_code": None,
            "error": "timed out; job continues, use logs()"}


@mcp.tool()
def exec_ephemeral(
    command: str,
    ttl: str = "2h",
    flavor: str | None = None,
    disk_gib: int | None = None,
    scratch: bool = False,
    topic: str | None = None,
    agent_id: str | None = None,
    nix_cache: bool = True,
    nix_auth: bool = True,
    profile: str | None = None,
    niks3_token: bool = True,
    arch: str | None = None,
    queue_timeout: int | None = None,
    sync_sources: list[str] | None = None,
    fetch_paths: list[str] | None = None,
    keep: bool = False,
    keep_on_failure: bool = False,
    timeout_seconds: int = 1800,
) -> dict:
    """Create a worker, run a command, fetch results and delete the worker.

    Set topic and agent_id as for up(). sync_sources holds SRC[:DEST] specs.
    fetch_paths holds REMOTE[:LOCAL] specs, fetched even on command failure.
    Set keep or keep_on_failure to retain a worker for debugging.
    Returns the worker name, command exit code, output and whether it was kept.
    """
    if timeout_seconds < 1:
        raise ToolError("Timeout must be positive")
    flags = provision_flags(ttl, flavor, disk_gib, scratch, topic, agent_id,
                            nix_cache, nix_auth, profile, niks3_token, arch, queue_timeout)
    for flag, paths in (("--sync", sync_sources), ("--fetch", fetch_paths)):
        for path in paths or []:
            flags += [flag, path]
    if keep:
        flags.append("--keep")
    if keep_on_failure:
        flags.append("--keep-on-failure")
    _, stdout, stderr, expired = run_bounded(
        "exec", "--json", *flags, "--", command, seconds=timeout_seconds,
    )
    if expired:
        raise ToolError("rworker exec timed out: " + tail(stderr.strip(), 4000)[0])
    try:
        result = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise command_error("exec", stdout, stderr) from error
    if not isinstance(result, dict) or "exit_code" not in result:
        raise command_error("exec", stdout, stderr)
    result["stdout"], stdout_truncated = tail(result["stdout"])
    result["stderr"], stderr_truncated = tail(result["stderr"])
    result["truncated"] = stdout_truncated or stderr_truncated
    return result


@mcp.tool()
def start(name: str, command: str, session: str | None = None,
          workdir: str | None = None) -> str:
    """Start a registered job. Returns its ID for jobs() and logs().

    session optionally names the job; omit it to generate a unique ID.
    """
    argv = ["start", name]
    if session:
        argv += ["--name", session]
    if workdir:
        argv += ["--workdir", workdir]
    return run(*argv, "--", command, timeout=60).strip()


@mcp.tool()
def jobs(name: str | None = None, structured: bool = False,
         watch_seconds: int | None = None, interval_seconds: int | None = None,
         limit: int = 10) -> str | list:
    """List registered jobs. structured returns JSON rows, or set watch_seconds
    to return up to limit structured snapshots over that period."""
    argv = ["jobs", *([name] if name else [])]
    if watch_seconds is not None:
        argv += ["--json", "--watch"]
        if interval_seconds is not None:
            argv += ["--interval", str(interval_seconds)]
        return observe(argv, watch_seconds, limit)
    if structured:
        return json.loads(run(*argv, "--json", timeout=60))
    return tail(run(*argv, timeout=60))[0]


@mcp.tool()
def ps(name: str | None = None, long: bool = False) -> str:
    """Show processes for running jobs on one or all reachable workers."""
    argv = ["ps"]
    if name:
        argv.append(name)
    if long:
        argv.append("--long")
    return tail(run(*argv, timeout=60))[0]


@mcp.tool()
def logs(name: str | None = None, session: str | None = None,
         lines: int = 200, cached: bool = False, redact: bool = False,
         redact_regex: str | None = None, topic: str | None = None,
         agent: str | None = None, agent_id: str | None = None,
         filters: list[str] | None = None, follow_seconds: int | None = None,
         no_events: bool = False, quiet: bool = False) -> str:
    """Read recent logs from one worker, one job, or all reachable workers.

    cached reads a retained snapshot and requires a worker name. The topic,
    agent and agent_id regex filters apply when listing all workers.
    redact masks common secrets. redact_regex also masks matching text.
    filters keeps lines matching any case-insensitive expression. Set
    follow_seconds for a bounded live tail (1–120 seconds).
    """
    if not name and (session or cached):
        raise ToolError("A worker name is required for a job or cached logs")
    if name and any(value is not None for value in (topic, agent, agent_id)):
        raise ToolError("Log filters are available when listing all workers")
    argv = ["logs", "--lines", str(lines)]
    if name:
        argv.append(name)
    if session:
        argv.append(session)
    if cached:
        argv.append("--cached")
    if redact:
        argv.append("--redact")
    for flag, value in (("--redact-regex", redact_regex), ("--topic", topic),
                        ("--agent", agent), ("--agent-id", agent_id)):
        if value is not None:
            argv += [flag, value]
    for pattern in filters or []:
        argv += ["--filter", pattern]
    if follow_seconds is not None:
        if cached or not 1 <= follow_seconds <= 120:
            raise ToolError("Live follow requires 1–120 seconds and cannot use cached logs")
        argv.append("--follow")
        if no_events:
            argv.append("--no-events")
        if quiet:
            argv.append("--quiet")
        code, stdout, stderr, expired = run_bounded(*argv, seconds=follow_seconds)
        if code and not expired:
            raise command_error(argv[0], stdout, stderr)
        return tail(stdout)[0]
    if no_events or quiet:
        raise ToolError("no_events and quiet require follow_seconds")
    return tail(run(*argv, timeout=60))[0]


@mcp.tool()
def events(topic: str | None = None, agent: str | None = None,
           agent_id: str | None = None, limit: int = 100,
           watch_seconds: int | None = None, since_now: bool = False,
           interval_seconds: int | None = None) -> list:
    """Read retained worker and job lifecycle events, newest entries last.

    topic, agent and agent_id accept regexes. Prefix a regex with ! to
    invert it. limit is the maximum number of events returned. Set
    watch_seconds for bounded live updates and since_now to skip history.
    """
    if limit < 1:
        raise ToolError("Event limit must be positive")
    argv = ["events", "--json"]
    for flag, value in (("--topic", topic), ("--agent", agent),
                        ("--agent-id", agent_id)):
        if value is not None:
            argv += [flag, value]
    if watch_seconds is not None:
        argv.append("--watch")
        if since_now:
            argv.append("--since-now")
        if interval_seconds is not None:
            argv += ["--interval", str(interval_seconds)]
        return observe(argv, watch_seconds, limit)
    if since_now:
        raise ToolError("since_now requires watch_seconds")
    return json.loads(run(*argv, timeout=60))[-limit:]


@mcp.tool()
def setup_register_log_file(name: str, job: str, log_path: str) -> str:
    """Register an existing absolute log-file path on a worker."""
    run("setup", "register-log-file", name, job, "--log", log_path, timeout=60)
    return job


@mcp.tool()
def sync(name: str, src: str, dest: str | None = None) -> dict:
    """rsync a local directory to the worker (mirrors with --delete; skips
    target/, .direnv/ and result). dest defaults to the directory's name in
    the agent's home."""
    argv = ["sync", "--json", name, src] + ([dest] if dest else [])
    return json.loads(run(*argv, timeout=1800))


@mcp.tool()
def cp(src: str, dest: str) -> dict:
    """Copy a file or directory to or from a worker using pinned SSH.

    Use WORKER:PATH for the remote side and a path on the MCP host for the local side.
    """
    return json.loads(run("cp", "--json", src, dest, timeout=1800))


@mcp.tool()
def ssh(name: str, command: str, user: str | None = None,
        identity: str | None = None, timeout_seconds: int = 600) -> dict:
    """Run a non-interactive command over pinned SSH and return its exit code and output.

    For work that should survive disconnection, use start() and logs().
    identity is an optional private key file path on the MCP host.
    """
    if timeout_seconds < 1:
        raise ToolError("Timeout must be positive")
    argv = ["ssh"]
    if user is not None:
        argv += ["-l", user]
    if identity is not None:
        argv += ["-i", identity]
    code, stdout, stderr, expired = run_bounded(
        *argv, name, "-T", command, seconds=timeout_seconds,
    )
    stdout, stdout_truncated = tail(stdout)
    stderr, stderr_truncated = tail(stderr)
    return {"exit_code": None if expired else code, "stdout": stdout,
            "stderr": stderr, "timed_out": expired,
            "truncated": stdout_truncated or stderr_truncated}


@mcp.tool()
def setup_nix(
    name: str,
    cache: bool = True,
    auth: bool = True,
    auth_hosts: list[str] | None = None,
    dry_run: bool = False,
) -> dict:
    """Get a worker ready to use this host's Nix (`rworker setup nix`).
    cache (default True): the private S3 Nix cache, i.e. this host's s3://
    substituters, the trusted public keys and the one AWS profile they name
    (read-only), so the worker substitutes from it. auth (default True): this
    host's Nix access-tokens (for example github.com), so the worker fetches
    private flake inputs itself instead of receiving them from here.
    auth_hosts limits the tokens to those hosts. Pushed over SSH into
    /run/rworker-nix on the worker; nothing else of nix.conf is copied. Run
    it again after a key or token was rotated. dry_run shows what would be
    pushed (never credentials or tokens). ok=false: see daemon
    (image-too-old, timeout) and reachable."""
    if not cache and not auth:
        raise ToolError("Enable at least one of cache or auth")
    argv = ["setup", "nix"]
    if not cache:
        argv.append("auth")
    elif not auth:
        argv.append("cache")
    argv.append("--json")
    for host in (auth_hosts or []) if auth else []:
        argv += ["--auth-host", host]
    if dry_run:
        argv.append("--dry-run")
    if cache and auth and not auth_hosts and not dry_run:
        argv.append("--")
    return json.loads(run(*argv, name, timeout=300))


@mcp.tool()
def setup_nix_upload_blob(
    name: str,
    files: list[str],
    store_name: str | None = None,
) -> dict:
    """Import local regular files into a worker's Nix store using flat SHA-256.

    Paths are on the MCP host. store_name overrides the basename for one file.
    Returns a paths array in upload order.
    """
    if not files or (store_name is not None and len(files) != 1):
        raise ToolError("Provide at least one file, or exactly one with store_name")
    argv = ["setup", "nix", "upload-blob", name, "--json"]
    if store_name is not None:
        argv += ["--name", store_name]
    return json.loads(run(*argv, "--", *files, timeout=1800))


@mcp.tool()
def setup_nix_builder(
    name: str = "",
    profile: str = "",
    arch: str = "",
    pool: str = "",
    identity: str = "",
    gateway: str | None = None,
    host_key: str | None = None,
    system: str | None = None,
    jobs: int | None = None,
    features: str | None = None,
    structured: bool = False,
) -> str | dict:
    """A Nix --builders line: nix build --max-jobs 0 --builders "<line>" .#pkg.
    With name, for that worker. Without it, for the on-demand builder
    gateway, which leases a worker on the first build and deletes it when
    idle; pick it with one of profile (e.g. large-builder, gh-aarch64), arch
    (x86_64, aarch64) or pool. identity is the private key the local Nix
    daemon uses for the gateway (default: the daemon's own).
    Set structured=True for the full object, including the SSH configuration
    required for a hosted worker (returned automatically for those workers)."""
    argv = ["setup", "nix", "builder"] + ([name] if name else [])
    for flag, value in (
        ("--profile", profile), ("--arch", arch), ("--pool", pool),
        ("--identity", identity), ("--gateway", gateway), ("--host-key", host_key),
        ("--system", system), ("--jobs", jobs), ("--features", features),
    ):
        if value is not None and value != "":
            argv += [flag, str(value)]
    result = json.loads(run(*argv, "--json", timeout=60))
    return result if structured or "ssh_config" in result else result["builder"]


def image_options(name, tag, cloud, project):
    argv = []
    for flag, value in (
        ("--name", name), ("--tag", tag), ("--cloud", cloud), ("--project", project),
    ):
        if value is not None:
            argv += [flag, value]
    return argv


@mcp.tool()
def image_create(
    flake: str | None = None, upload: bool = False, name: str | None = None,
    tag: str | None = None, cloud: str | None = None, project: str | None = None,
    timeout_seconds: int = 7200,
) -> dict:
    """Build a Nix worker disk image on the MCP host, optionally uploading it to OpenStack.

    Use a build host. flake optionally includes #ATTR. Upload options require upload=True.
    Returns the image's store path and, if uploaded, its OpenStack name and ID.
    """
    argv = ["image", "create", "--json"]
    if flake is not None:
        argv += ["--flake", flake]
    if upload:
        argv.append("--upload")
    return json.loads(run(*argv, *image_options(name, tag, cloud, project), timeout=timeout_seconds))


@mcp.tool()
def image_upload(
    path: str, name: str, tag: str | None = None, cloud: str | None = None,
    project: str | None = None, timeout_seconds: int = 3600,
) -> dict:
    """Upload a local .raw, .img or .qcow2 file as a private OpenStack image."""
    return json.loads(run(
        "image", "upload", "--json", *image_options(name, tag, cloud, project),
        "--", path, timeout=timeout_seconds,
    ))


@mcp.tool()
def cli_help(action: str | None = None) -> str:
    """Read rworker help, for an action or for the CLI as a whole."""
    return run("help", *([action] if action else []), timeout=30)


@mcp.tool()
def completions(shell: str) -> str:
    """Return the CLI completion script for bash or zsh."""
    return run("completions", shell, timeout=30)


if __name__ == "__main__":
    mcp.run()
