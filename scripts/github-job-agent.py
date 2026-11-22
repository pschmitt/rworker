"""Run one controller assignment on a Linux CI runner."""

from contextlib import ExitStack, contextmanager
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
            with request(url, token, payload, timeout=min(15, remaining)):
                return True
        except urllib.error.HTTPError as error:
            if error.code == 403 and error.headers.get("X-Rworker-Lease") == "closed":
                return False
            if error.code not in (403, 429, 500, 502, 503, 504):
                raise
        except (urllib.error.URLError, TimeoutError):
            pass
        time.sleep(max(0, min(5, deadline - time.time())))
    if time.time() >= expires_at:
        return False
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


@contextmanager
def gitlab_nix_daemon():
    with subprocess.Popen(["nix-daemon"], stdin=subprocess.DEVNULL, start_new_session=True) as daemon:
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
        if encryption_enabled(profile):
            stack.enter_context(encrypted_build_storage(profile))
        if profile in GITLAB_PROFILES:
            if LEASE_IMAGE:
                stack.enter_context(gitlab_nix_daemon())
            else:
                prepare_gitlab_nix()
        yield kvm_capabilities()


def prepare_sshd(paths, public_key):
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
DisableForwarding yes
PermitUserEnvironment no
SetEnv XDG_CONFIG_HOME={pwd.getpwnam(BUILD_USER).pw_dir}/.config PATH=/nix/var/nix/profiles/default/bin:/usr/local/bin:/usr/bin:/bin{trust}
AcceptEnv LANG LC_*
Subsystem sftp internal-sftp
"""
    )
    return sshd_config, host_key


def start_ssh_worker(url, token, assignment):
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
        sshd_config, host_key = prepare_sshd(paths, assignment["public_key"])
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
                endpoint = {"port": int(port), "host_key": host_key.with_suffix(".pub").read_text().strip()}
                if not update_lease(f"{url}/ready", token, endpoint, expires_at):
                    return
                while update_lease(f"{url}/heartbeat", token, {}, expires_at):
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
        yield


def check_runtime(profile):
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
    with build_environment(profile) as capabilities, tempfile.TemporaryDirectory(dir="/tmp") as directory:
        print(f"rworker runtime KVM: {'kvm' in capabilities}", flush=True)
        if encryption_enabled(profile):
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
                expression = (
                    'builtins.derivation { name = "rworker-runtime-check"; '
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

    with build_environment(profile) as capabilities:
        with request(
            f"{url}/claim",
            oidc_token(profile),
            {
                "profile": profile,
                "architecture": architecture,
                "capabilities": capabilities,
                "disk": build_disk(),
            },
        ) as response:
            assignment = json.load(response)
        start_ssh_worker(url, assignment["token"], assignment)
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
