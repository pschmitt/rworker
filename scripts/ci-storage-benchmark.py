"""Run the same workloads inside either CI lease runtime over SSH."""

from hashlib import sha256
from pathlib import Path
from statistics import median
from time import monotonic
import subprocess
import sys
import tempfile


def timed(command):
    started = monotonic()
    subprocess.run(command, check=True)
    return monotonic() - started


def benchmark_disk():
    size_mib = 256
    with tempfile.TemporaryDirectory(dir=Path.home()) as directory:
        path = Path(directory) / "data"
        writes, reads = [], []
        for _ in range(3):
            writes.append(timed([
                "dd", "if=/dev/zero", f"of={path}", "bs=1M", f"count={size_mib}",
                "oflag=direct", "conv=fdatasync", "status=none",
            ]))
            reads.append(timed([
                "dd", f"if={path}", "of=/dev/null", "bs=1M", "iflag=direct",
                "status=none",
            ]))
        print(f"rworker benchmark write_mib_s={size_mib / median(writes):.1f}", flush=True)
        print(f"rworker benchmark read_mib_s={size_mib / median(reads):.1f}", flush=True)


def benchmark_nix(revision):
    expression = """
    let
      inherit (builtins) currentSystem derivation;
      data = derivation {
        name = "rworker-bench-data";
        system = currentSystem;
        builder = "/bin/sh";
        args = [ "-c" "/bin/dd if=/dev/zero of=$out bs=1M count=64 status=none" ];
      };
    in derivation {
      name = "rworker-bench-readback";
      system = currentSystem;
      builder = "/bin/sh";
      args = [ "-c" "/bin/sha256sum < ${data} > $out" ];
    }
    """
    nix = ["nix", "build", "--impure", "--no-link", "--print-out-paths"]
    started = monotonic()
    output = subprocess.check_output([*nix, "--offline", "--expr", expression], text=True)
    elapsed = monotonic() - started
    digest = sha256()
    block = bytes(1024**2)
    for _ in range(64):
        digest.update(block)
    if Path(output.strip()).read_text().split()[0] != digest.hexdigest():
        raise RuntimeError("Nix build output checksum mismatch")
    print(f"rworker benchmark local_nix_s={elapsed:.3f}", flush=True)
    elapsed = timed([*nix, "--rebuild", f"github:NixOS/nixpkgs/{revision}#hello"])
    print(f"rworker benchmark hello_source_s={elapsed:.3f}", flush=True)


if __name__ == "__main__":
    benchmark_disk()
    benchmark_nix(sys.argv[1])
