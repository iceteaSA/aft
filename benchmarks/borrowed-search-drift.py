#!/usr/bin/env python3
"""Measure read-only borrowed-index search latency as worktree drift grows.

The harness builds an owner index for a synthetic Git repository, creates sibling
worktrees with exactly 0, 250, and 1,000 same-size edits, then issues literal
searches through the owner process's external-path lane. Output is NDJSON so raw
samples and load observations remain machine-readable.
"""

from __future__ import annotations

import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

DRIFT_COUNTS = (0, 250, 1_000)
DEFAULT_FILE_COUNT = 4_000
DEFAULT_FILE_SIZE = 128 * 1024
DEFAULT_REPEATS = 7

JsonObject = dict[str, Any]


class AftClient:
    def __init__(self, binary: Path, stderr_path: Path) -> None:
        self.stderr_file = stderr_path.open("wb")
        self.process = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            bufsize=0,
        )
        self.buffer = b""
        self.next_id = 0

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.stderr_file.close()

    def call(self, command: str, timeout_secs: float = 300.0, **params: Any) -> JsonObject:
        self.next_id += 1
        request_id = str(self.next_id)
        request = {"id": request_id, "command": command, **params}
        if self.process.stdin is None:
            raise RuntimeError("AFT stdin is unavailable")
        self.process.stdin.write(
            (json.dumps(request, separators=(",", ":")) + "\n").encode()
        )
        self.process.stdin.flush()

        deadline = time.time() + timeout_secs
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"AFT exited with code {self.process.returncode}")
            if self.process.stdout is None:
                raise RuntimeError("AFT stdout is unavailable")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.1)
            if ready:
                chunk = os.read(self.process.stdout.fileno(), 65_536)
                if chunk:
                    self.buffer += chunk
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    continue
                if frame.get("id") == request_id:
                    return frame
        raise TimeoutError(f"timed out waiting for {command} response {request_id}")


def run(*args: str, cwd: Path | None = None) -> None:
    subprocess.run(args, cwd=cwd, check=True)


def uptime() -> str:
    return subprocess.check_output(["uptime"], text=True).strip()


def emit(value: JsonObject) -> None:
    print(json.dumps(value, separators=(",", ":")), flush=True)


def initialize_owner(owner: Path, file_count: int, file_size: int) -> None:
    owner.mkdir(parents=True)
    run("git", "init", "-q", str(owner))
    run("git", "config", "user.email", "borrow-bench@example.invalid", cwd=owner)
    run("git", "config", "user.name", "Borrow benchmark", cwd=owner)

    # A linked worktree has a root-level .git control file. Excluding it keeps
    # the requested drift count independent of Git's worktree representation.
    (owner / ".aftignore").write_text(".git\n")
    for index in range(file_count):
        tokens = []
        if index < 40:
            tokens.append("probe_0000")
        if index < 250:
            tokens.append("probe_0250")
        if index < 1_000:
            tokens.append("probe_1000")
        prefix = (" ".join(tokens) + "\n").encode()
        line = (f"synthetic source file {index:04d} " + "x" * 96 + "\n").encode()
        repeats = (file_size - len(prefix)) // len(line) + 1
        (owner / f"file_{index:04d}.txt").write_bytes(
            (prefix + line * repeats)[:file_size]
        )

    run("git", "add", ".", cwd=owner)
    run("git", "commit", "-qm", "synthetic owner corpus", cwd=owner)


def synchronize_tracked_mtimes(owner: Path, worktree: Path, file_count: int) -> None:
    relative_paths = [Path(".aftignore")]
    relative_paths.extend(Path(f"file_{index:04d}.txt") for index in range(file_count))
    for relative_path in relative_paths:
        owner_stat = (owner / relative_path).stat()
        os.utime(
            worktree / relative_path,
            ns=(owner_stat.st_atime_ns, owner_stat.st_mtime_ns),
        )


def create_borrow_worktrees(base: Path, owner: Path, file_count: int) -> list[Path]:
    roots = []
    for drift_count in DRIFT_COUNTS:
        worktree = base / f"borrow-{drift_count}"
        run(
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            f"borrow-bench-{drift_count}",
            str(worktree),
            cwd=owner,
        )
        synchronize_tracked_mtimes(owner, worktree, file_count)

        if drift_count:
            old_token = f"probe_{drift_count:04d}".encode()
            new_token = f"stale_{drift_count:04d}".encode()
            for index in range(drift_count):
                path = worktree / f"file_{index:04d}.txt"
                owner_stat = (owner / path.name).stat()
                content = path.read_bytes()
                if old_token not in content:
                    raise RuntimeError(f"missing probe token in {path}")
                path.write_bytes(content.replace(old_token, new_token, 1))
                # Preserve size and mtime so strict hashing, rather than a cheap
                # metadata mismatch, is what discovers each changed file.
                os.utime(path, ns=(owner_stat.st_atime_ns, owner_stat.st_mtime_ns))
        roots.append(worktree)
    return roots


def configure_owner(client: AftClient, owner: Path, storage: Path) -> None:
    config_document = json.dumps(
        {
            "search_index": True,
            "semantic_search": False,
            "callgraph_store": False,
            "inspect": {"enabled": False},
        }
    )
    response = client.call(
        "configure",
        project_root=str(owner),
        harness="opencode",
        storage_dir=str(storage),
        config=[
            {
                "tier": "user",
                "source": "/tmp/borrow-search-bench-aft.jsonc",
                "doc": config_document,
            }
        ],
    )
    if not response.get("success"):
        raise RuntimeError(f"configure failed: {response}")

    deadline = time.time() + 300
    while time.time() < deadline:
        status = client.call("status")
        search_status = status.get("search_index") or {}
        if search_status.get("status") == "ready":
            emit({"event": "owner_ready", "search_index": search_status, "uptime": uptime()})
            return
        time.sleep(0.2)
    raise TimeoutError("owner search index did not become ready")


def measure(
    client: AftClient,
    drift_count: int,
    worktree: Path,
    event: str,
    repeat: int,
) -> JsonObject:
    observed_uptime = uptime()
    started = time.perf_counter()
    response = client.call(
        "semantic_search",
        query=f"probe_{drift_count:04d}",
        hint="literal",
        top_k=20,
        include_tests=True,
        path=str(worktree),
    )
    latency_ms = (time.perf_counter() - started) * 1_000
    row = {
        "event": event,
        "drift_requested": drift_count,
        "repeat": repeat,
        "latency_ms": round(latency_ms, 3),
        "drift_observed": response.get("drift_count"),
        "result_count": len(response.get("results") or []),
        "success": response.get("success"),
        "uptime": observed_uptime,
    }
    emit(row)
    return row


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True, help="release AFT binary")
    parser.add_argument(
        "--work-dir",
        type=Path,
        help="new directory for the synthetic repository (temporary by default)",
    )
    parser.add_argument("--keep", action="store_true", help="retain the generated corpus")
    parser.add_argument("--file-count", type=int, default=DEFAULT_FILE_COUNT)
    parser.add_argument("--file-size", type=int, default=DEFAULT_FILE_SIZE)
    parser.add_argument("--repeats", type=int, default=DEFAULT_REPEATS)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    binary = args.binary.expanduser().resolve()
    if not binary.is_file():
        raise FileNotFoundError(binary)
    if args.file_count < max(DRIFT_COUNTS):
        raise ValueError(f"--file-count must be at least {max(DRIFT_COUNTS)}")
    if args.file_size < 64:
        raise ValueError("--file-size must be at least 64 bytes")
    if args.repeats < 1:
        raise ValueError("--repeats must be positive")

    generated_temp = args.work_dir is None
    base = (
        Path(tempfile.mkdtemp(prefix="aft-borrow-search-"))
        if generated_temp
        else args.work_dir.expanduser().resolve()
    )
    if not generated_temp:
        if base.exists():
            raise FileExistsError(f"work directory already exists: {base}")
        base.mkdir(parents=True)

    client: AftClient | None = None
    try:
        owner = base / "owner"
        storage = base / "storage"
        initialize_owner(owner, args.file_count, args.file_size)
        worktrees = create_borrow_worktrees(base, owner, args.file_count)
        client = AftClient(binary, base / "aft.stderr")
        configure_owner(client, owner, storage)

        for drift_count, worktree in zip(DRIFT_COUNTS, worktrees):
            measure(client, drift_count, worktree, "warmup", 0)

        rows = []
        cases = list(zip(DRIFT_COUNTS, worktrees))
        for repeat in range(1, args.repeats + 1):
            order = cases if repeat % 2 else list(reversed(cases))
            for drift_count, worktree in order:
                rows.append(measure(client, drift_count, worktree, "measurement", repeat))

        for drift_count in DRIFT_COUNTS:
            values = [
                row["latency_ms"]
                for row in rows
                if row["drift_requested"] == drift_count
            ]
            sorted_values = sorted(values)
            emit(
                {
                    "event": "summary",
                    "drift_requested": drift_count,
                    "raw_ms": values,
                    "median_ms": sorted_values[len(sorted_values) // 2],
                    "min_ms": sorted_values[0],
                    "max_ms": sorted_values[-1],
                }
            )
        emit({"event": "complete", "work_dir": str(base), "kept": args.keep})
        return 0
    finally:
        if client is not None:
            client.close()
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileNotFoundError, FileExistsError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
