#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
image=${AFT_SANDBOX_LINUX_IMAGE:-rust:1.97-bookworm}

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is unavailable; install Docker and rerun $0" >&2
  exit 69
fi

# Docker's default seccomp profile may reject the Landlock syscalls before the
# kernel can report its ABI, so this spike removes that outer syscall filter.
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

    echo "Landlock feature probe:"
    /tmp/aft-sandbox-s1-target/debug/aft sandbox-launch --support

    echo "Runnable probe battery:"
    cargo test --quiet -p agent-file-tools --test sandbox_launch_probe -- --test-threads=1 --nocapture

    echo "Expected-verdict probes that nono 0.68.0 cannot satisfy on Linux:"
    for probe in \
      p4_nested_project_write_denies_are_enforced \
      p5_secret_read_is_denied_and_other_reads_are_allowed \
      p8_docker_and_agent_socket_connects_are_denied
    do
      if cargo test --quiet -p agent-file-tools --test sandbox_launch_probe "${probe}" -- --ignored --exact --nocapture; then
        echo "${probe}: unexpectedly matched the desired verdict"
      else
        echo "${probe}: expected-verdict assertion failed (recorded spike finding)"
      fi
    done
  '
