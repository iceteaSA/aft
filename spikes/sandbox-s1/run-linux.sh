#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image=${AFT_SANDBOX_LINUX_IMAGE:-rust:1.97-bookworm}

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is unavailable; install Docker and rerun $0" >&2
  exit 69
fi

# Docker's default seccomp profile may reject the Landlock syscalls before the
# kernel can report its ABI, so the runner removes that outer syscall filter.
docker run --rm \
  --security-opt seccomp=unconfined \
  --mount "type=bind,source=${repo_root},target=/work,readonly" \
  --workdir /work \
  --env CARGO_TARGET_DIR=/tmp/aft-sandbox-s1-target \
  "${image}" \
  bash -c '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    if ! command -v python3 >/dev/null 2>&1; then
      apt-get update -qq
      apt-get install -y -qq python3
    fi

    echo "kernel: $(uname -srvm)"
    cargo build --quiet -p agent-file-tools --bin aft
    cargo build --quiet --release -p agent-file-tools --bin aft

    echo "Landlock support probe:"
    /tmp/aft-sandbox-s1-target/debug/aft sandbox-launch --support

    echo "Linux first-party probe battery:"
    # P1/P2/P3/P6 assert the write allowlist; P4/P5/P8 assert ALLOWED plus
    # the structured unenforced warning; P7 records hardlinks; P9 covers PTY.
    cargo test --quiet -p agent-file-tools --test sandbox_launch_probe -- \
      --test-threads=1 --nocapture

    echo "Release-build P10 overhead:"
    cargo test --quiet --release -p agent-file-tools \
      --test sandbox_launch_probe \
      p10_launcher_latency_delta_is_measured_over_twenty_iterations -- \
      --exact --nocapture
  '
