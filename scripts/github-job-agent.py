"""Run one controller assignment on a Linux CI runner."""

from contextlib import ExitStack, contextmanager
import base64
import grp
import json
import os
import platform
import pwd
import re
import signal
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from uuid import uuid4

GITLAB_PROFILES = {"gl-linux-x64", "gl-linux-x64-lease"}
ARCHITECTURES = {"linux-arm64": "aarch64", "linux-x64": "x86_64"}
ARCHITECTURES.update(dict.fromkeys(GITLAB_PROFILES, "x86_64"))
LEASE_IMAGE = Path("/etc/rworker/lease-image").is_file()
AUDIENCE = "rworker-job"
BUILD_USER = "rworker-build"
NIX_BUILDER_PATH = ".local/bin/rworker-nix-daemon"
# Where `rworker setup nix` and `setup niks3-token` put the client's Nix
# settings and the lease's niks3 token, relative to the build user's home.
NIX_SETTINGS = ".config/rworker-nix"
NIKS3_SETTINGS = ".config/rworker-niks3"
# Pushes to the private Nix cache with the lease's own token, through the
# lease's Nix (the runner has no niks3).
NIKS3_WRAPPER = """#!/bin/sh
d="$HOME/.config/rworker-niks3"
if [ -r "$d/token" ]; then
  NIKS3_AUTH_TOKEN_FILE="$d/token"
  export NIKS3_AUTH_TOKEN_FILE
fi
if [ -r "$d/server-url" ]; then
  NIKS3_SERVER_URL=$(cat "$d/server-url")
  export NIKS3_SERVER_URL
fi
exec nix shell nixpkgs#niks3 -c niks3 "$@"
"""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file, code, message, headers, url):
        return None


HTTP = urllib.request.build_opener(NoRedirect())


def request(url, token, payload=None, *, timeout=15):
    if urllib.parse.urlsplit(url).scheme != "https":
        raise ValueError("Agent endpoints must use HTTPS")
    data = None if payload is None else json.dumps(payload).encode()
    return HTTP.open(
        urllib.request.Request(
            url,
            data=data,
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
        ),
        timeout=timeout,
    )


def update_lease(url, token, payload, expires_at):
    deadline = min(expires_at, time.time() + 60)
    while (remaining := deadline - time.time()) > 0:
        try:
            with request(url, token, payload, timeout=remaining) as response:
                return json.load(response) if response.status == 200 else {}
        except urllib.error.HTTPError as error:
            if error.code == 403 and error.headers.get("X-Rworker-Lease") == "closed":
                return None
            if error.code not in (403, 429, 500, 502, 503, 504):
                raise
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(max(0, min(5, deadline - time.time())))
    if time.time() >= expires_at:
        return None
    raise RuntimeError("Lost contact with the rworker controller")


def oidc_token(profile):
    if profile in GITLAB_PROFILES:
        return os.environ.pop("RWORKER_GITLAB_ID_TOKEN")
    url = os.environ["ACTIONS_ID_TOKEN_REQUEST_URL"]
    separator = "&" if "?" in url else "?"
    with request(
        f"{url}{separator}audience={AUDIENCE}",
        os.environ["ACTIONS_ID_TOKEN_REQUEST_TOKEN"],
    ) as response:
        return json.load(response)["value"]


def privileged(*command):
    return list(command) if os.geteuid() == 0 else ["sudo", "-n", *command]


def run_as_build_user(command, **kwargs):
    if os.geteuid() != 0:
        return subprocess.run(["sudo", "-n", "-u", BUILD_USER, "--", *command], **kwargs)
    user = pwd.getpwnam(BUILD_USER)
    env = {"PATH": os.environ["PATH"], "HOME": user.pw_dir, "USER": BUILD_USER}
    if "SSL_CERT_FILE" in os.environ:
        env["SSL_CERT_FILE"] = os.environ["SSL_CERT_FILE"]
    return subprocess.run(
        command, user=user.pw_uid, group=user.pw_gid,
        extra_groups=os.getgrouplist(BUILD_USER, user.pw_gid), env=env, **kwargs,
    )


def encryption_enabled(profile):
    opt_out = os.getenv("RWORKER_UNENCRYPTED_PROFILES", "").split(",")
    return profile not in {item.strip() for item in opt_out}


def prepare_build_user():
    if not LEASE_IMAGE:
        subprocess.run(
            privileged("useradd", "--user-group", "--create-home", "--shell", "/bin/bash", BUILD_USER),
            check=True,
        )
    device = Path("/dev/kvm")
    if not device.is_char_device():
        return []
    # Access follows the runner device's group. Probe VM creation under the
    # same identity that runs submitted commands.
    try:
        kvm_group = grp.getgrnam("kvm")
    except KeyError:
        kvm_group = None
    if kvm_group is not None and device.stat().st_gid == kvm_group.gr_gid:
        subprocess.run(
            privileged("usermod", "-aG", "kvm", BUILD_USER),
            check=True,
        )


def kvm_capabilities():
    if not Path("/dev/kvm").is_char_device():
        return []
    probe = """
import fcntl
import os

with open('/dev/kvm', 'r+b', buffering=0) as device:
    vm = fcntl.ioctl(device, 0xAE01, 0)  # KVM_CREATE_VM
    os.close(vm)
"""
    result = run_as_build_user(
        [sys.executable, "-c", probe],
        capture_output=True,
        check=False,
    )
    return ["kvm"] if result.returncode == 0 else []


def build_user_home():
    return Path(pwd.getpwnam(BUILD_USER).pw_dir)


def install_build_user_file(relative, content, mode="0700"):
    run_as_build_user(
        ["/bin/bash", "-eu", "-c", 'umask 077; mkdir -p "${1%/*}"; cat > "$1"; chmod "$2" "$1"',
         "rworker-install", str(build_user_home() / relative), mode],
        input=content, text=True, check=True,
    )


def prepare_session_tools():
    install_build_user_file(".local/bin/niks3", NIKS3_WRAPPER)


def prepare_gitlab_nix():
    # Only the build user owns the store in this disposable container.
    subprocess.run(["install", "-d", "-o", BUILD_USER, "-g", BUILD_USER, "/nix/var/nix"], check=True)
    subprocess.run(["chown", "-R", f"{BUILD_USER}:{BUILD_USER}", "/nix"], check=True)
    config_directory = Path("/etc/nix")
    config_directory.mkdir(exist_ok=True)
    with (config_directory / "nix.conf").open("a") as config:
        config.write(
            "\nexperimental-features = nix-command flakes\nbuild-users-group =\nsandbox = false\n"
        )
    run_as_build_user(["nix-store", "--init"], check=True)


def prepare_github_nix_builder():
    # Remote Nix clients need write permission to the store. Give the lease
    # its own store, under the build user's home and separate from the runner
    # daemon and the Nix tools used by the agent.
    daemon = Path("/nix/var/nix/profiles/default/bin/nix-daemon")
    restriction = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    if restriction.exists() and restriction.read_text().strip() == "1":
        # Attach to the daemon executable so Ubuntu permits its build
        # namespaces even when unprivileged profile transitions are disabled.
        profile = f'''abi <abi/4.0>,
profile rworker-nix-builder "{daemon.resolve(strict=True)}" flags=(unconfined) {{
  userns,
}}
'''
        subprocess.run(privileged("apparmor_parser", "--replace"), input=profile, text=True, check=True)
    # The daemon also reads what `setup nix` pushed (substituters, access
    # tokens and the AWS credentials of the S3 cache), so a client that
    # builds through it with builders-use-substitutes substitutes there too.
    # NIX_REMOTE would point the daemon at itself.
    install_build_user_file(
        NIX_BUILDER_PATH,
        f"""#!/bin/sh
unset NIX_REMOTE
export NIX_CONFIG="build-users-group =
sandbox = true"
c="$HOME/{NIX_SETTINGS}"
NIX_USER_CONF_FILES="$HOME/.config/nix/nix.conf:$c/nix.conf"
export NIX_USER_CONF_FILES
if [ -r "$c/aws-credentials" ]; then
  AWS_SHARED_CREDENTIALS_FILE="$c/aws-credentials"
  export AWS_SHARED_CREDENTIALS_FILE
fi
exec {daemon} --store "$HOME/.local/share/nix/rworker" "$@"
""",
    )


@contextmanager
def gitlab_nix_daemon():
    # The build user is a client of this daemon. To use the cache and tokens
    # `setup nix` pushes (substituters are only honoured from a trusted user)
    # it has to be trusted, and the daemon, which does the substituting, reads
    # the AWS credentials from the file `setup nix` writes. The container is
    # the build user's alone.
    with Path("/etc/nix/nix.conf").open("a") as config:
        config.write(f"\ntrusted-users = root {BUILD_USER}\n")
    environment = dict(
        os.environ,
        AWS_SHARED_CREDENTIALS_FILE=str(build_user_home() / NIX_SETTINGS / "aws-credentials"),
    )
    with subprocess.Popen(
        ["nix-daemon"], stdin=subprocess.DEVNULL, start_new_session=True, env=environment,
    ) as daemon:
        try:
            ready = Path("/nix/var/nix/daemon-socket/socket")
            deadline = time.monotonic() + 10
            while not ready.is_socket():
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Nix daemon did not start")
                time.sleep(0.05)
            yield
        finally:
            try:
                os.killpg(daemon.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass


@contextmanager
def build_environment(profile):
    prepare_build_user()
    with ExitStack() as stack:
        volume = stack.enter_context(encrypted_build_storage(profile)) if encryption_enabled(profile) else None
        if profile in GITLAB_PROFILES:
            if LEASE_IMAGE:
                stack.enter_context(gitlab_nix_daemon())
            else:
                prepare_gitlab_nix()
        else:
            prepare_github_nix_builder()
        prepare_session_tools()
        yield kvm_capabilities(), volume


def prepare_sshd(paths, public_key, proxy_environment=()):
    host_key = paths / "host-key"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", host_key], check=True)
    authorized = paths / "authorized"
    authorized.write_text(public_key + "\n")
    authorized_path = "/etc/ssh/rworker-authorized"
    subprocess.run(
        privileged("install", "-m", "0644", authorized, authorized_path), check=True
    )
    subprocess.run(privileged("install", "-d", "-m", "0755", "/run/sshd"), check=True)
    if not LEASE_IMAGE:
        # pam_limits applies the session limits after sshd starts.
        limits = paths / "limits"
        limits.write_text(f"{BUILD_USER} - stack 65536\n")
        subprocess.run(
            privileged("install", "-m", "0644", limits, "/etc/security/limits.d/99-rworker-build.conf"),
            check=True,
        )
    # The runner image may mark newly created passwordless accounts locked.
    # PAM permits public-key login while password-based methods stay disabled.
    sshd_config = paths / "sshd_config"
    trust = " SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" if LEASE_IMAGE else ""
    # Sessions find the niks3 command, and Nix and the AWS SDK find what
    # `setup nix` pushed (files that may not exist: both skip them).
    home = build_user_home()
    nix_settings = home / NIX_SETTINGS
    session = (
        f" NIX_USER_CONF_FILES={home}/.config/nix/nix.conf:{nix_settings}/nix.conf"
        f" AWS_SHARED_CREDENTIALS_FILE={nix_settings}/aws-credentials"
    )
    proxies = "".join(f" {name}={value}" for name, value in proxy_environment)
    sshd_config.write_text(
        f"""ListenAddress 127.0.0.1
Port 22222
HostKey {host_key}
AuthorizedKeysFile {authorized_path}
AllowUsers {BUILD_USER}
UsePAM {"no" if LEASE_IMAGE else "yes"}
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
# Agent forwarding only: builds fetch private inputs with the client's key
# without the key ever being copied here. Nothing else is forwarded.
AllowAgentForwarding yes
AllowTcpForwarding no
AllowStreamLocalForwarding no
X11Forwarding no
PermitTunnel no
PermitUserEnvironment no
SetEnv XDG_CONFIG_HOME={home}/.config PATH={home}/.local/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:/usr/bin:/bin{session}{trust}{proxies}
AcceptEnv LANG LC_*
Subsystem sftp internal-sftp
"""
    )
    return sshd_config, host_key


def start_ssh_worker(url, token, assignment, mesh):
    gateway = os.environ["RWORKER_GATEWAY"]
    if not re.fullmatch(r"[a-z_][a-z0-9_-]*@[a-zA-Z0-9][a-zA-Z0-9.-]*", gateway):
        raise ValueError("Invalid SSH gateway address")
    with tempfile.TemporaryDirectory(prefix="rworker-ssh-", dir="/tmp") as directory:
        paths = Path(directory)
        identity = paths / "gateway-key"
        identity.write_text(os.environ.pop("RWORKER_GATEWAY_KEY").strip() + "\n")
        identity.chmod(0o600)
        known_hosts = paths / "known_hosts"
        known_hosts.write_text(os.environ["RWORKER_GATEWAY_KNOWN_HOSTS"].strip() + "\n")
        sshd_config, host_key = prepare_sshd(paths, assignment["public_key"], mesh.proxies())
        with subprocess.Popen(
            privileged("/usr/sbin/sshd", "-D", "-e", "-f", sshd_config),
            stdin=subprocess.DEVNULL,
        ) as sshd:
            master = [
                "ssh", "-F", "/dev/null", "-S", str(paths / "gateway"),
                "-o", "BatchMode=yes", "-o", "ConnectTimeout=15",
                "-o", "StrictHostKeyChecking=yes", "-o", "IdentitiesOnly=yes",
                "-o", f"UserKnownHostsFile={known_hosts}", "-i", str(identity),
            ]
            try:
                subprocess.run([*master, "-fMN", gateway], check=True, timeout=25)
                port = subprocess.check_output(
                    [*master, "-O", "forward", "-R", "127.0.0.1:0:127.0.0.1:22222", gateway],
                    text=True, timeout=20,
                ).strip()
                if sshd.poll() is not None:
                    raise RuntimeError("Runner SSH server stopped")
                expires_at = assignment["expires_at"]
                endpoint = {
                    "port": int(port),
                    "host_key": " ".join(host_key.with_suffix(".pub").read_text().split()[:2]),
                }
                if update_lease(f"{url}/ready", token, endpoint, expires_at) is None:
                    return
                result = {}
                while (update := update_lease(f"{url}/heartbeat", token, result, expires_at)) is not None:
                    result = {}
                    if update:
                        result = {"mesh_result": mesh.configure(update)}
                    else:
                        time.sleep(5)
                    if sshd.poll() is not None:
                        raise RuntimeError("Runner SSH server stopped")
                    subprocess.run([*master, "-O", "check", gateway], check=True, timeout=10,
                                   stderr=subprocess.DEVNULL)
            finally:
                try:
                    subprocess.run([*master, "-O", "exit", gateway], check=False, timeout=10)
                finally:
                    if sshd.poll() is None:
                        # sudo forwards SIGTERM to the privileged sshd.
                        sshd.terminate()
                    subprocess.run(
                        privileged("pkill", "-KILL", "-u", BUILD_USER), check=False
                    )

def build_disk():
    storage = os.statvfs(pwd.getpwnam(BUILD_USER).pw_dir)
    return {
        "total_bytes": storage.f_blocks * storage.f_frsize,
        # Older controllers require this field when decoding the claim.
        "available_bytes": storage.f_bavail * storage.f_frsize,
    }


@contextmanager
def encrypted_volume(parent, size):
    with tempfile.TemporaryDirectory(prefix="rworker-volume-", dir=parent) as directory:
        paths = Path(directory)
        backing = paths / "disk"
        with backing.open("wb") as disk:
            disk.truncate(size)
        mountpoint = paths / "data"
        mountpoint.mkdir()
        name = f"rworker-{uuid4().hex}"
        device = f"/dev/mapper/{name}"
        key = os.urandom(64)
        options = ["--batch-mode", "--type", "luks2", "--key-file", "-"]
        # A uniformly random 512-bit input needs no memory-hard password stretching.
        subprocess.run(privileged(
            "cryptsetup", "luksFormat", *options, "--pbkdf", "pbkdf2",
            "--pbkdf-force-iterations", "10000", backing,
        ), input=key, check=True)
        subprocess.run(
            privileged("cryptsetup", "open", *options, backing, name), input=key, check=True,
        )
        del key
        deferred = False
        try:
            with ExitStack() as stack:
                subprocess.run(privileged("mkfs.ext4", "-q", "-m", "0", "-E", "nodiscard", device), check=True)
                mount_volume(stack, mountpoint, "-o", "nodev,nosuid", device)
                # The kernel drops the key after the last reference closes.
                subprocess.run(privileged("cryptsetup", "close", "--deferred", name), check=True)
                deferred = True
                backing.unlink()
                yield mountpoint
        finally:
            if not deferred:
                subprocess.run(privileged("cryptsetup", "close", name), check=True)


def unmount(path):
    if subprocess.run(privileged("umount", path), check=False).returncode:
        subprocess.run(privileged("umount", "-l", path), check=True)


def mount_volume(stack, path, *arguments):
    subprocess.run(privileged("mount", *arguments, path), check=True)
    stack.callback(unmount, path)


@contextmanager
def encrypted_build_storage(profile):
    for program in ("cryptsetup", "mkfs.ext4", "mount", "umount"):
        if not shutil.which(program):
            raise RuntimeError(f"Encrypted lease storage requires {program}")
    parent = Path(os.environ.get("RUNNER_TEMP", os.environ.get("CI_PROJECT_DIR", ".")))
    space = os.statvfs(parent)
    size = min((space.f_bavail * space.f_frsize) - 1024**3, 256 * 1024**3)
    if size < 1024**3:
        raise RuntimeError("Encrypted lease storage requires at least 2 GiB free")
    github = profile not in GITLAB_PROFILES
    with ExitStack() as stack:
        volume = stack.enter_context(encrypted_volume(parent, size))
        nix = Path("/nix")
        subprocess.run(privileged("mkdir", "-p", nix), check=True)
        upper, work = volume / "nix-upper", volume / "nix-work"
        subprocess.run(privileged("mkdir", upper, work), check=True)
        home = pwd.getpwnam(BUILD_USER)
        for name, path, mode, uid, gid in (
            ("home", home.pw_dir, "0700", home.pw_uid, home.pw_gid),
            ("tmp", "/tmp", "1777", 0, 0),
            ("var-tmp", "/var/tmp", "1777", 0, 0),
        ):
            subprocess.run(privileged("mkdir", "-p", path), check=True)
            directory = volume / name
            subprocess.run(privileged("install", "-d", "-m", mode, "-o", str(uid), "-g", str(gid), directory), check=True)
            mount_volume(stack, path, "--bind", directory)
        if github:
            subprocess.run(privileged("systemctl", "stop", "nix-daemon.socket", "nix-daemon.service"), check=True)
            stack.callback(subprocess.run, privileged("systemctl", "start", "nix-daemon.socket"), check=True)
        mount_volume(stack, nix, "-t", "overlay", "overlay", "-o", f"lowerdir={nix},upperdir={upper},workdir={work},index=off")
        if github:
            subprocess.run(privileged("systemctl", "start", "nix-daemon.socket"), check=True)
            stack.callback(subprocess.run, privileged("systemctl", "stop", "nix-daemon.socket", "nix-daemon.service"), check=True)
        print("rworker lease storage: encrypted with LUKS2", flush=True)
        yield volume


def verify_encrypted_writes(volume, output, marker):
    probe = """
import os
from pathlib import Path
import sqlite3
import sys

volume, output = map(Path, sys.argv[1:3])
marker = sys.argv[3]
device = volume.stat().st_dev
dm_uuid = Path(f"/sys/dev/block/{os.major(device)}:{os.minor(device)}/dm/uuid")
if not dm_uuid.read_text().startswith("CRYPT-LUKS2-"):
    raise SystemExit("Storage probe filesystem is not backed by LUKS2")

upper = volume / "nix-upper"
database = upper / "var/nix/db/db.sqlite"
for path in (upper / output.relative_to("/nix"), *(volume / name / marker for name in ("home", "tmp", "var-tmp"))):
    if path.stat().st_dev != device or path.read_text().strip() != "ready":
        raise SystemExit("Storage probe write missing from the LUKS2 filesystem")
if database.stat().st_dev != device:
    raise SystemExit("Nix database is outside the LUKS2 filesystem")
with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
    if connection.execute("SELECT 1 FROM ValidPaths WHERE path = ?", (str(output),)).fetchone() is None:
        raise SystemExit("New Nix store path is missing from the encrypted database")
"""
    subprocess.run(privileged(sys.executable, "-c", probe, volume, output, marker), check=True)
    print("Storage probe verified LUKS2 backing for new Nix output, database entry, home and temp files", flush=True)


def check_runtime(profile):
    benchmark = os.getenv("RWORKER_RUNTIME_BENCHMARK", "off")
    if benchmark not in {"off", "luks", "plain"}:
        raise ValueError("Unknown runtime benchmark mode")
    if benchmark != "off":
        os.environ["RWORKER_UNENCRYPTED_PROFILES"] = profile if benchmark == "plain" else ""
    started = time.monotonic()
    status = Path("/proc/self/status").read_text().splitlines()
    capabilities = next(line.split()[1] for line in status if line.startswith("CapEff:"))
    print(f"Storage probe CAP_SYS_ADMIN: {bool(int(capabilities, 16) & (1 << 21))}", flush=True)
    for device in ("/dev/loop-control", "/dev/mapper/control", "/dev/fuse", "/dev/kvm"):
        try:
            fd = os.open(device, os.O_RDWR | os.O_CLOEXEC)
        except OSError as error:
            print(f"Storage probe {device}: {error.strerror}", flush=True)
        else:
            os.close(fd)
            print(f"Storage probe {device}: accessible", flush=True)
    subprocess.run(
        ["stat", "-f", "-c", "Storage probe %n: %T", ".", "/nix", "/dev/shm"],
        check=False,
    )
    with build_environment(profile) as (capabilities, volume), tempfile.TemporaryDirectory(dir="/tmp") as directory:
        if benchmark != "off":
            elapsed = time.monotonic() - started
            print(f"rworker benchmark {profile} {benchmark} setup_s={elapsed:.3f}", flush=True)
        print(f"rworker runtime KVM: {'kvm' in capabilities}", flush=True)
        if volume is not None:
            for mount in (pwd.getpwnam(BUILD_USER).pw_dir, "/tmp", "/var/tmp", "/nix"):
                if not os.path.ismount(mount):
                    raise RuntimeError(f"Missing encrypted storage mount at {mount}")
        paths = Path(directory)
        key = paths / "client"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
        config, host_key = prepare_sshd(paths, key.with_suffix(".pub").read_text().strip())
        known_hosts = paths / "known_hosts"
        known_hosts.write_text("[127.0.0.1]:22222 " + host_key.with_suffix(".pub").read_text())
        with subprocess.Popen(privileged("/usr/sbin/sshd", "-D", "-e", "-f", config)) as daemon:
            try:
                deadline = time.monotonic() + 5
                while True:
                    try:
                        with socket.create_connection(("127.0.0.1", 22222), timeout=1):
                            break
                    except ConnectionRefusedError:
                        if daemon.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError("Runtime SSH server did not start") from None
                        time.sleep(0.05)
                ssh = [
                    "ssh", "-F", "/dev/null", "-p", "22222", "-i", str(key),
                    "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
                    "-o", f"UserKnownHostsFile={known_hosts}", f"{BUILD_USER}@127.0.0.1",
                ]
                marker = f"rworker-runtime-check-{uuid4().hex}"
                expression = (
                    f'builtins.derivation {{ name = "{marker}"; '
                    'system = builtins.currentSystem; builder = "/bin/sh"; '
                    'args = [ "-c" "echo ready > $out" ]; }'
                )
                command = (
                    "id -un && nix build --offline --impure --no-link --print-out-paths --expr "
                    + shlex.quote(expression)
                )
                output = subprocess.check_output([*ssh, command], text=True, timeout=30).splitlines()
                if len(output) != 2 or output[0] != BUILD_USER:
                    raise RuntimeError("Runtime SSH user mismatch")
                if Path(output[1]).read_text().strip() != "ready":
                    raise RuntimeError("Runtime Nix build produced unexpected output")
                if profile not in GITLAB_PROFILES:
                    check_github_nix_builder(key, host_key, ARCHITECTURES[profile] + "-linux", marker)
                if volume is not None:
                    subprocess.run([
                        *ssh, f'printf ready > "$HOME/{marker}" && '
                        f"printf ready > /tmp/{marker} && printf ready > /var/tmp/{marker}",
                    ], check=True, timeout=30)
                    verify_encrypted_writes(volume, output[1], marker)
                if benchmark != "off":
                    lock = json.loads(Path("flake.lock").read_text())
                    revision = lock["nodes"]["nixpkgs"]["locked"]["rev"]
                    script = Path(__file__).with_name("ci-storage-benchmark.py")
                    subprocess.run(
                        [*ssh, f"python3 - {shlex.quote(revision)}"],
                        input=script.read_bytes(), check=True, timeout=900,
                    )
                if os.getenv("RWORKER_CHECK_HTTPS") == "1":
                    subprocess.run([
                        *ssh,
                        "openssl version && curl -fLsS --max-time 15 "
                        "https://channels.nixos.org/flake-registry.json >/dev/null "
                        "&& nix registry list >/dev/null",
                    ], check=True, timeout=30)
            finally:
                if daemon.poll() is None:
                    daemon.terminate()
    print("rworker runtime check passed", flush=True)


def check_github_nix_builder(key, host_key, system, marker):
    public_key = " ".join(host_key.with_suffix(".pub").read_text().split()[:2]) + "\n"
    host_public_key = base64.b64encode(public_key.encode()).decode()
    home = pwd.getpwnam(BUILD_USER).pw_dir
    store = f"ssh-ng://{BUILD_USER}@127.0.0.1:22222?remote-program={home}/{NIX_BUILDER_PATH}"
    builder = f"{store} {system} {key} 1 1 - - {host_public_key}"
    revision = json.loads(Path("flake.lock").read_text())["nodes"]["nixpkgs"]["locked"]["rev"]
    expression = (
        f'let pkgs = (builtins.getFlake "github:NixOS/nixpkgs/{revision}").legacyPackages.{system}; '
        f'in pkgs.runCommand "{marker}-distributed" {{ }} "echo ready > $out"'
    )
    output = subprocess.check_output(
        ["nix", "build", "--impure", "--max-jobs", "0", "--builders", builder,
         "--option", "builders-use-substitutes", "true", "--no-link", "--print-out-paths",
         "--expr", expression],
        text=True, timeout=300,
    ).strip()
    if Path(output).read_text().strip() != "ready":
        raise RuntimeError("Distributed Nix build produced unexpected output")


def mesh_tools(providers):
    lock = json.loads(Path("flake.lock").read_text())
    revision = lock["nodes"]["nixpkgs"]["locked"]["rev"]
    paths = subprocess.check_output([
        "nix", "build", "--no-link", "--print-out-paths",
        *(f"github:NixOS/nixpkgs/{revision}#{provider}" for provider in providers),
    ], text=True, timeout=180).splitlines()
    return {path.name: path for directory in paths for path in (Path(directory) / "bin").iterdir()}


def stop_mesh_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def wait_for_mesh(process, provider, address):
    deadline = time.monotonic() + 45
    while process.poll() is None and time.monotonic() < deadline:
        try:
            with socket.create_connection(address, timeout=1):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"{provider} mesh proxy did not start")


@contextmanager
def mesh_peer(provider, peer, hostname, address, tools):
    with ExitStack() as stack:
        directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="rworker-mesh-", dir="/dev/shm")))
        key = directory / f"{provider}.key"
        with os.fdopen(os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as handle:
            handle.write(peer.pop("key"))
        host, port = address
        proxy = f"{host}:{port}"
        env = {name: os.environ[name] for name in ("PATH", "SSL_CERT_FILE") if name in os.environ}
        env["HOME"] = str(directory)
        if provider == "tailscale":
            control = directory / "tailscale.sock"
            daemon = [
                tools["tailscaled"], "--tun=userspace-networking", "--state=mem:",
                f"--statedir={directory}", f"--socket={control}", f"--socks5-server={proxy}",
            ]
        else:
            env.update(
                NB_USE_NETSTACK_MODE="true",
                NB_STATE_DIR=str(directory),
                NB_SOCKS5_LISTENER_ADDRESS=host,
                NB_SOCKS5_LISTENER_PORT=str(port),
            )
            daemon = [
                tools["netbird"], "up", "--foreground-mode", "--no-browser", "--block-inbound",
                "--log-file", "console",
                "--management-url", peer["management_url"], "--setup-key-file", key,
                "--hostname", hostname,
            ]
        process = subprocess.Popen(
            daemon, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        stack.callback(stop_mesh_process, process)
        wait_for_mesh(process, provider, address)
        if provider == "tailscale":
            subprocess.run([
                tools["tailscale"], f"--socket={control}", "up",
                f"--auth-key=file:{key}", f'--hostname={hostname}',
                "--accept-dns=false", "--accept-routes=false", "--shields-up",
            ], check=True, timeout=45, env=env, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        key.unlink()
        print(f"rworker mesh proxy ready: {provider}", flush=True)
        yield


class Mesh:
    def __init__(self):
        self.ports = {}
        self.clients = {}
        self.tools = {}

    def start(self, provider, peer, hostname):
        if provider not in self.tools:
            self.tools[provider] = mesh_tools([provider])
        if provider not in self.ports:
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                self.ports[provider] = listener.getsockname()[1]
        if previous := self.clients.pop(provider, None):
            previous.close()
        with ExitStack() as client:
            client.enter_context(mesh_peer(
                provider, peer, hostname,
                ("127.0.0.1", self.ports[provider]), self.tools[provider],
            ))
            self.clients[provider] = client.pop_all()
        return self.ports[provider]

    def close(self):
        for client in self.clients.values():
            client.close()

    def proxies(self):
        return [
            (f"RWORKER_{provider.upper()}_PROXY", f"socks5h://127.0.0.1:{port}")
            for provider, port in self.ports.items()
        ]

    def configure(self, request):
        provider = request["provider"]
        mesh = request["mesh"]
        port = None
        try:
            port = self.start(provider, mesh[provider], mesh["hostname"])
        except (OSError, RuntimeError, KeyError, ValueError, subprocess.SubprocessError):
            print(f"rworker mesh setup failed: {provider}", file=sys.stderr, flush=True)
        return {"id": request["id"], "port": port}


@contextmanager
def mesh_environment(assignment):
    initial = assignment.pop("mesh", {})
    with ExitStack() as stack:
        mesh = Mesh()
        stack.callback(mesh.close)
        for provider in ("tailscale", "netbird"):
            if provider in initial:
                mesh.start(provider, initial[provider], initial["hostname"])
        yield mesh


def main():
    profile = os.environ["RWORKER_PROFILE"]
    architecture = platform.machine()
    if ARCHITECTURES.get(profile) != architecture:
        raise ValueError("Runner architecture does not match the requested profile")
    if (profile == "gl-linux-x64-lease") != LEASE_IMAGE:
        raise ValueError("Container image does not match the requested profile")
    if sys.argv[1:] == ["--check-runtime"]:
        check_runtime(profile)
        return 0
    job_id = os.environ["RWORKER_JOB_ID"]
    if not re.fullmatch(r"[a-f0-9]{32}", job_id):
        raise ValueError("Invalid controller assignment ID")
    if profile in GITLAB_PROFILES:
        job_id = f"glci-{job_id}"
    base_url = os.environ["RWORKER_AGENT_URL"].rstrip("/")
    url = f"{base_url}/v1/agent/jobs/{job_id}"

    with build_environment(profile) as (capabilities, _):
        capabilities.append("mesh-v1")
        # The Ubuntu GitLab profile gives the build user control of /nix.
        # A running build could replace an executable there before enrollment.
        if profile not in GITLAB_PROFILES or LEASE_IMAGE:
            capabilities.append("mesh-setup-v1")
        with request(
            f"{url}/claim",
            oidc_token(profile),
            {
                "profile": profile,
                "architecture": architecture,
                "capabilities": capabilities,
                "disk": build_disk(),
            },
            timeout=60,
        ) as response:
            assignment = json.load(response)
        with mesh_environment(assignment) as mesh:
            start_ssh_worker(url, assignment["token"], assignment, mesh)
    return 0


if __name__ == "__main__":
    def terminate(_signum, _frame):
        raise SystemExit(143)

    signal.signal(signal.SIGTERM, terminate)
    try:
        sys.exit(main())
    except Exception as error:
        detail = f": {error}" if type(error) is RuntimeError else ""
        print(f"rworker agent failed ({type(error).__name__}){detail}", file=sys.stderr)
        sys.exit(1)
