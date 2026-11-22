"""Run one controller assignment on a Linux CI runner."""

import grp
import json
import os
import platform
import pwd
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ARCHITECTURES = {"linux-arm64": "aarch64", "linux-x64": "x86_64", "gl-linux-x64": "x86_64"}
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
    if profile == "gl-linux-x64":
        return os.environ.pop("RWORKER_GITLAB_ID_TOKEN")
    url = os.environ["ACTIONS_ID_TOKEN_REQUEST_URL"]
    separator = "&" if "?" in url else "?"
    with request(
        f"{url}{separator}audience={AUDIENCE}",
        os.environ["ACTIONS_ID_TOKEN_REQUEST_TOKEN"],
    ) as response:
        return json.load(response)["value"]


def prepare_build_user():
    subprocess.run(
        ["sudo", "-n", "useradd", "--user-group", "--create-home", "--shell", "/bin/bash", BUILD_USER],
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
            ["sudo", "-n", "usermod", "-aG", "kvm", BUILD_USER],
            check=True,
        )
    probe = """
import fcntl
import os

with open('/dev/kvm', 'r+b', buffering=0) as device:
    vm = fcntl.ioctl(device, 0xAE01, 0)  # KVM_CREATE_VM
    os.close(vm)
"""
    result = subprocess.run(
        ["sudo", "-n", "-u", BUILD_USER, "--", "/usr/bin/python3", "-c", probe],
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
    subprocess.run(["sudo", "-n", "-u", BUILD_USER, "--", "nix-store", "--init"], check=True)


def start_ssh_worker(url, token, assignment):
    gateway = os.environ["RWORKER_GATEWAY"]
    if not re.fullmatch(r"[a-z_][a-z0-9_-]*@[a-zA-Z0-9][a-zA-Z0-9.-]*", gateway):
        raise ValueError("Invalid SSH gateway address")
    with tempfile.TemporaryDirectory(prefix="rworker-ssh-") as directory:
        paths = Path(directory)
        identity = paths / "gateway-key"
        identity.write_text(os.environ.pop("RWORKER_GATEWAY_KEY").strip() + "\n")
        identity.chmod(0o600)
        known_hosts = paths / "known_hosts"
        known_hosts.write_text(os.environ["RWORKER_GATEWAY_KNOWN_HOSTS"].strip() + "\n")
        host_key = paths / "host-key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", host_key], check=True)
        authorized = paths / "authorized"
        authorized.write_text(assignment["public_key"] + "\n")
        authorized_path = "/etc/ssh/rworker-authorized"
        subprocess.run(
            ["sudo", "-n", "install", "-m", "0644", authorized, authorized_path], check=True
        )
        subprocess.run(["sudo", "-n", "install", "-d", "-m", "0755", "/run/sshd"], check=True)
        # pam_limits applies the session limits after sshd starts.
        limits = paths / "limits"
        limits.write_text(f"{BUILD_USER} - stack 65536\n")
        subprocess.run(
            ["sudo", "-n", "install", "-m", "0644", limits,
             "/etc/security/limits.d/99-rworker-build.conf"],
            check=True,
        )
        # The runner image may mark newly created passwordless accounts locked.
        # PAM permits public-key login while password-based methods stay disabled.
        sshd_config = paths / "sshd_config"
        sshd_config.write_text(
            f"""ListenAddress 127.0.0.1
Port 22222
HostKey {host_key}
AuthorizedKeysFile {authorized_path}
AllowUsers {BUILD_USER}
UsePAM yes
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
DisableForwarding yes
PermitUserEnvironment no
SetEnv XDG_CONFIG_HOME={pwd.getpwnam(BUILD_USER).pw_dir}/.config
SetEnv PATH=/nix/var/nix/profiles/default/bin:/usr/local/bin:/usr/bin:/bin
AcceptEnv LANG LC_*
Subsystem sftp internal-sftp
"""
        )
        with subprocess.Popen(
            ["sudo", "-n", "/usr/sbin/sshd", "-D", "-e", "-f", sshd_config],
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
                        ["sudo", "-n", "pkill", "-KILL", "-u", BUILD_USER], check=False
                    )

def build_disk():
    storage = os.statvfs(pwd.getpwnam(BUILD_USER).pw_dir)
    return {
        "total_bytes": storage.f_blocks * storage.f_frsize,
        # Older controllers require this field when decoding the claim.
        "available_bytes": storage.f_bavail * storage.f_frsize,
    }


def main():
    profile = os.environ["RWORKER_PROFILE"]
    architecture = platform.machine()
    if ARCHITECTURES.get(profile) != architecture:
        raise ValueError("Runner architecture does not match the requested profile")
    job_id = os.environ["RWORKER_JOB_ID"]
    if not re.fullmatch(r"[a-f0-9]{32}", job_id):
        raise ValueError("Invalid controller assignment ID")
    if profile == "gl-linux-x64":
        job_id = f"glci-{job_id}"
    base_url = os.environ["RWORKER_AGENT_URL"].rstrip("/")
    url = f"{base_url}/v1/agent/jobs/{job_id}"

    capabilities = prepare_build_user()
    if profile == "gl-linux-x64":
        prepare_gitlab_nix()
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
    try:
        sys.exit(main())
    except Exception as error:
        print(f"rworker agent failed ({type(error).__name__})", file=sys.stderr)
        sys.exit(1)
