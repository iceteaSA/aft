#!/usr/bin/env python3
"""Serial warm-query latency comparison for AFT's index and ripgrep.

Examples:
  python3 benchmarks/grep-glob-vs-rg.py --aft after=target/release/aft
  python3 benchmarks/grep-glob-vs-rg.py \
    --aft before=/tmp/aft-before --aft after=target/release/aft \
    --corpus aft=. --corpus openclaw=~/Work/OSS/openclaw --output /tmp/results.json

AFT is measured through its persistent NDJSON protocol. ripgrep is measured as the
requested CLI baseline. Runs are serial, perform warmups before samples, and refuse
to start when the one-minute load average exceeds --max-load-per-cpu.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import queue
import re
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


GREP_QUERIES = [
    ("common_identifier", "SearchIndex", True, None),
    ("common_identifier", "Result", True, None),
    ("common_identifier", "config", True, None),
    ("common_identifier", "path", True, None),
    ("rare_identifier", "artifact_cache_key", True, None),
    ("rare_identifier", "SearchIndexSnapshot", True, None),
    ("rare_identifier", "canonicalize_or_normalize", True, None),
    ("rare_identifier", "walk_truncated", True, None),
    ("regex_literal_core", r"Search(Index|Scope)", True, None),
    ("regex_literal_core", r"fn\s+handle_\w+", True, None),
    ("regex_literal_core", r"Result<[^>]+>", True, None),
    ("regex_literal_core", r"perf\s+\w+\s+phases", True, None),
    ("path_scoped", "use", True, "src"),
    ("path_scoped", "pub", True, "src"),
    ("path_scoped", "test", True, "tests"),
    ("path_scoped", "config", True, "packages"),
    ("case_insensitive", "searchindex", False, None),
    ("case_insensitive", "result", False, None),
    ("case_insensitive", "configuration", False, None),
    ("case_insensitive", "pathbuf", False, None),
]

GLOB_QUERIES = [
    ("extension", "**/*.rs"),
    ("extension", "**/*.ts"),
    ("extension", "**/*.json"),
    ("directory", "crates/**/*"),
    ("directory", "packages/**/*"),
    ("rare", "**/*search_index*"),
    ("rare", "**/*parity*"),
    ("rare", "**/Cargo.toml"),
]

PHASE_RE = re.compile(
    r"perf grep phases: snapshot_acquire=(?P<snapshot>[0-9.]+)ms "
    r"trigram_lookup=(?P<trigram>[0-9.]+)ms pread_verify=(?P<pread>[0-9.]+)ms "
    r"candidates=(?P<candidates>\d+) bytes=(?P<bytes>\d+) "
    r"post_filter/format=(?P<post>[0-9.]+)ms scope_probe=(?P<scope>[0-9.]+)ms"
)
GLOB_PHASE_RE = re.compile(
    r"perf glob phases: source=(?P<source>\S+) walk=(?P<walk>[0-9.]+)ms "
    r"entries_visited=(?P<entries>\d+) scope_probe=(?P<scope>[0-9.]+)ms "
    r"discovery_total=(?P<total>[0-9.]+)ms"
)


@dataclass(frozen=True)
class NamedPath:
    name: str
    path: Path


def named_path(value: str) -> NamedPath:
    if "=" not in value:
        raise argparse.ArgumentTypeError("expected NAME=PATH")
    name, raw_path = value.split("=", 1)
    path = Path(raw_path).expanduser().resolve()
    if not name or not path.exists():
        raise argparse.ArgumentTypeError(f"invalid NAME=PATH: {value}")
    return NamedPath(name, path)


def percentile(samples: list[float], fraction: float) -> float:
    ordered = sorted(samples)
    index = max(0, min(len(ordered) - 1, int((len(ordered) - 1) * fraction + 0.999999)))
    return ordered[index]


def stats(samples: list[float]) -> dict[str, float]:
    return {
        "p50_ms": round(statistics.median(samples), 3),
        "p90_ms": round(percentile(samples, 0.90), 3),
        "mean_ms": round(statistics.mean(samples), 3),
    }


def machine_load() -> dict[str, Any]:
    load = os.getloadavg()
    return {
        "load_1m": round(load[0], 2),
        "load_5m": round(load[1], 2),
        "load_15m": round(load[2], 2),
        "logical_cpus": os.cpu_count() or 1,
    }


def require_quiet(max_load_per_cpu: float) -> dict[str, Any]:
    load = machine_load()
    ratio = load["load_1m"] / load["logical_cpus"]
    if ratio > max_load_per_cpu:
        raise RuntimeError(
            f"host is contended: load1/cpu={ratio:.2f} exceeds {max_load_per_cpu:.2f}; rerun later "
            "or deliberately raise --max-load-per-cpu and report the load"
        )
    return load


class AftClient:
    def __init__(self, binary: Path, corpus: Path, phase_logs: bool) -> None:
        self._storage = tempfile.TemporaryDirectory(prefix="aft-grep-glob-bench-")
        env = os.environ.copy()
        env["AFT_STORAGE_DIR"] = self._storage.name
        if phase_logs:
            env["RUST_LOG"] = "agent_file_tools=debug,aft=debug"
        self.proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        self._id = 0
        self._stdout: queue.Queue[str] = queue.Queue()
        self._stderr: queue.SimpleQueue[str] = queue.SimpleQueue()
        threading.Thread(target=self._drain_stdout, daemon=True).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        response = self.call(
            "configure",
            {
                "project_root": str(corpus),
                "harness": "runner",
                "config": [
                    {
                        "tier": "user",
                        "doc": json.dumps(
                            {
                                "search_index": True,
                                "semantic_search": False,
                                "callgraph_store": False,
                            }
                        ),
                    }
                ],
                "_bypass_size_limits": True,
            },
            timeout=120.0,
        )
        if not response.get("success"):
            raise RuntimeError(f"AFT configure failed: {response}")

    def _drain_stdout(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            self._stdout.put(line)

    def _drain_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            self._stderr.put(line.rstrip())

    def call(self, command: str, params: dict[str, Any], timeout: float = 30.0) -> dict[str, Any]:
        self._id += 1
        request = {"id": str(self._id), "command": command, **params}
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"AFT {command} timed out")
            if self.proc.poll() is not None and self._stdout.empty():
                raise RuntimeError(f"AFT exited with {self.proc.returncode}")
            try:
                line = self._stdout.get(timeout=min(remaining, 0.25))
            except queue.Empty:
                continue
            response = json.loads(line)
            if str(response.get("id")) == str(self._id):
                return response

    def wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        status = "unknown"
        while time.monotonic() < deadline:
            response = self.call("grep", {"pattern": "SearchIndex", "max_results": 1}, timeout=60.0)
            status = str(response.get("index_status", "unknown"))
            if status.lower() == "ready":
                return
            time.sleep(0.25)
        raise TimeoutError(f"search index did not become ready (last status={status})")

    def phase_lines(self) -> list[str]:
        lines: list[str] = []
        while True:
            try:
                lines.append(self._stderr.get_nowait())
            except queue.Empty:
                return lines

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self._storage.cleanup()


def existing_scope(corpus: Path, requested: str | None) -> Path | None:
    if requested is None:
        return None
    candidate = corpus / requested
    return candidate if candidate.exists() else None


def bench_aft(
    binary: Path,
    corpus: Path,
    iterations: int,
    warmups: int,
    ready_timeout: float,
) -> dict[str, Any]:
    client = AftClient(binary, corpus, phase_logs=True)
    try:
        client.wait_ready(ready_timeout)
        grep_rows = []
        for query_class, pattern, case_sensitive, requested_scope in GREP_QUERIES:
            scope = existing_scope(corpus, requested_scope)
            params: dict[str, Any] = {
                "pattern": pattern,
                "case_sensitive": case_sensitive,
                "max_results": 100,
            }
            if scope is not None:
                params["path"] = str(scope)
            for _ in range(warmups):
                client.call("grep", params)
            roundtrip: list[float] = []
            bridge: list[float] = []
            for _ in range(iterations):
                started = time.perf_counter()
                response = client.call("grep", params)
                roundtrip.append((time.perf_counter() - started) * 1000.0)
                bridge.append(float(response.get("search_ms", 0.0)))
            grep_rows.append(
                {
                    "class": query_class,
                    "pattern": pattern,
                    "scope": str(scope.relative_to(corpus)) if scope else None,
                    "roundtrip": stats(roundtrip),
                    "bridge_search": stats(bridge),
                }
            )

        glob_rows = []
        for query_class, pattern in GLOB_QUERIES:
            params = {"pattern": pattern}
            for _ in range(warmups):
                client.call("glob", params)
            samples = []
            for _ in range(iterations):
                started = time.perf_counter()
                client.call("glob", params)
                samples.append((time.perf_counter() - started) * 1000.0)
            glob_rows.append({"class": query_class, "pattern": pattern, "roundtrip": stats(samples)})

        time.sleep(0.05)
        phases = parse_phases(client.phase_lines())
        return {"grep": grep_rows, "glob": glob_rows, "phases": phases}
    finally:
        client.close()


def parse_phases(lines: list[str]) -> dict[str, Any]:
    grep_values: dict[str, list[float]] = {
        key: [] for key in ("snapshot", "trigram", "pread", "post", "scope", "candidates", "bytes")
    }
    glob_values: dict[str, list[float]] = {
        key: [] for key in ("walk", "scope", "total", "entries")
    }
    glob_sources: dict[str, int] = {}
    for line in lines:
        match = PHASE_RE.search(line)
        if match:
            for key in grep_values:
                grep_values[key].append(float(match.group(key)))
        glob_match = GLOB_PHASE_RE.search(line)
        if glob_match:
            for key in glob_values:
                glob_values[key].append(float(glob_match.group(key)))
            source = glob_match.group("source")
            glob_sources[source] = glob_sources.get(source, 0) + 1
    return {
        "grep": {key: stats(values) for key, values in grep_values.items() if values},
        "glob": {key: stats(values) for key, values in glob_values.items() if values},
        "glob_sources": glob_sources,
    }


def run_rg(command: list[str], corpus: Path) -> float:
    started = time.perf_counter()
    subprocess.run(command, cwd=corpus, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    return (time.perf_counter() - started) * 1000.0


def bench_rg(corpus: Path, iterations: int, warmups: int) -> dict[str, Any]:
    grep_rows = []
    for query_class, pattern, case_sensitive, requested_scope in GREP_QUERIES:
        scope = existing_scope(corpus, requested_scope)
        command = ["rg", "--color", "never", "--line-number", "--column", "--max-count", "100"]
        if not case_sensitive:
            command.append("--ignore-case")
        command.append(pattern)
        if scope:
            command.append(str(scope))
        for _ in range(warmups):
            run_rg(command, corpus)
        samples = [run_rg(command, corpus) for _ in range(iterations)]
        grep_rows.append(
            {
                "class": query_class,
                "pattern": pattern,
                "scope": str(scope.relative_to(corpus)) if scope else None,
                "roundtrip": stats(samples),
            }
        )

    glob_rows = []
    for query_class, pattern in GLOB_QUERIES:
        command = ["rg", "--files", "-g", pattern]
        for _ in range(warmups):
            run_rg(command, corpus)
        samples = [run_rg(command, corpus) for _ in range(iterations)]
        glob_rows.append({"class": query_class, "pattern": pattern, "roundtrip": stats(samples)})
    return {"grep": grep_rows, "glob": glob_rows}


def summarize_classes(rows: list[dict[str, Any]], metric: str = "roundtrip") -> dict[str, Any]:
    grouped: dict[str, list[dict[str, float]]] = {}
    for row in rows:
        grouped.setdefault(row["class"], []).append(row[metric])
    return {
        query_class: {
            "p50_ms": round(statistics.median(item["p50_ms"] for item in values), 3),
            "p90_ms": round(percentile([item["p90_ms"] for item in values], 0.90), 3),
        }
        for query_class, values in grouped.items()
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aft", action="append", type=named_path, default=[], metavar="NAME=PATH")
    parser.add_argument("--corpus", action="append", type=named_path, default=[], metavar="NAME=PATH")
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--ready-timeout", type=float, default=900.0)
    parser.add_argument("--max-load-per-cpu", type=float, default=0.75)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not args.aft:
        parser.error("at least one --aft NAME=PATH is required")
    if not args.corpus:
        args.corpus = [NamedPath("repo", Path.cwd().resolve())]
    if subprocess.run(["rg", "--version"], stdout=subprocess.DEVNULL).returncode != 0:
        parser.error("rg is required")

    try:
        load = require_quiet(args.max_load_per_cpu)
    except RuntimeError as error:
        print(error, file=sys.stderr)
        return 2

    report: dict[str, Any] = {
        "metadata": {
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "platform": platform.platform(),
            "iterations": args.iterations,
            "warmups": args.warmups,
            "load": load,
        },
        "corpora": {},
    }
    for corpus in args.corpus:
        print(f"\n[{corpus.name}] {corpus.path}", flush=True)
        corpus_result: dict[str, Any] = {"aft": {}}
        for aft in args.aft:
            print(f"  AFT {aft.name}: {aft.path}", flush=True)
            result = bench_aft(aft.path, corpus.path, args.iterations, args.warmups, args.ready_timeout)
            result["grep_classes"] = summarize_classes(result["grep"])
            result["glob_classes"] = summarize_classes(result["glob"])
            corpus_result["aft"][aft.name] = result
        print("  ripgrep CLI", flush=True)
        rg_result = bench_rg(corpus.path, args.iterations, args.warmups)
        rg_result["grep_classes"] = summarize_classes(rg_result["grep"])
        rg_result["glob_classes"] = summarize_classes(rg_result["glob"])
        corpus_result["rg"] = rg_result
        report["corpora"][corpus.name] = corpus_result

    encoded = json.dumps(report, indent=2)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded + "\n")
        print(f"\nwrote {args.output}")
    else:
        print(encoded)
    return 0


if __name__ == "__main__":
    sys.exit(main())
