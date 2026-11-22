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
        args = [ "-c" ''
          block=0
          n=0
          while [ "$n" -lt 20 ]; do
            block="$block$block"
            n=$((n + 1))
          done
          n=0
          while [ "$n" -lt 64 ]; do
            printf '%s' "$block"
            n=$((n + 1))
          done > "$out"
        '' ];
      };
    in derivation {
      name = "rworker-bench-readback";
      system = currentSystem;
      builder = "/bin/sh";
      args = [ "-c" "printf '%s' '${data}' > \"$out\"" ];
    }
    """
    nix = ["nix", "build", "--impure", "--no-link", "--print-out-paths"]
    started = monotonic()
    output = subprocess.check_output([*nix, "--offline", "--expr", expression], text=True)
    elapsed = monotonic() - started
    data = Path(Path(output.strip()).read_text())
    expected, actual = sha256(), sha256()
    with data.open("rb") as handle:
        for block in iter(lambda: handle.read(1024**2), b""):
            actual.update(block)
    for _ in range(64):
        expected.update(b"0" * 1024**2)
    if expected.digest() != actual.digest():
        raise RuntimeError("Nix build output checksum mismatch")
    print(f"rworker benchmark local_nix_s={elapsed:.3f}", flush=True)
    elapsed = timed([*nix, "--rebuild", f"github:NixOS/nixpkgs/{revision}#hello"])
    print(f"rworker benchmark hello_source_s={elapsed:.3f}", flush=True)


if __name__ == "__main__":
    benchmark_disk()
    benchmark_nix(sys.argv[1])
