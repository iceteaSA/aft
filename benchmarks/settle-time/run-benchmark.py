#!/usr/bin/env python3
"""Measure AFT configure-to-settle time for one GitHub repository."""
from __future__ import annotations

import argparse
import csv
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

TIER2_CATEGORIES = ("dead_code", "unused_exports", "duplicates")
TERMINAL_INDEX_STATES = {"ready", "disabled", "failed"}


def run(cmd: list[str], cwd: Path | None = None, timeout: int | None = None) -> subprocess.CompletedProcess[str]:
    print("$", " ".join(cmd), flush=True)
    return subprocess.run(cmd, cwd=cwd, timeout=timeout, check=True, text=True, capture_output=True)


def command_output(cmd: list[str], cwd: Path | None = None) -> str:
    try:
        return subprocess.run(cmd, cwd=cwd, check=False, text=True, capture_output=True).stdout.strip()
    except Exception as error:  # pragma: no cover - best-effort metadata only
        return f"unavailable: {error}"


def repo_slug(url: str) -> str:
    parsed = urlparse(url)
    path = parsed.path if parsed.scheme else url
    parts = [part for part in path.strip("/").split("/") if part]
    if len(parts) >= 2:
        owner, name = parts[-2], parts[-1]
    else:
        owner, name = "repo", parts[-1] if parts else "repo"
    if name.endswith(".git"):
        name = name[:-4]
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", f"{owner}-{name}")
    return safe.strip("-") or "repo"


def clone_repo(url: str, ref: str | None, dest: Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    if ref:
        run(["git", "init", str(dest)])
        run(["git", "remote", "add", "origin", url], cwd=dest)
        try:
            run(["git", "fetch", "--depth", "1", "origin", ref], cwd=dest, timeout=3600)
        except subprocess.CalledProcessError:
            run(["git", "fetch", "--depth", "1", "origin", f"+{ref}:{ref}"], cwd=dest, timeout=3600)
        run(["git", "checkout", "--detach", "FETCH_HEAD"], cwd=dest)
    else:
        run(["git", "clone", "--depth", "1", url, str(dest)], timeout=3600)


def git_lines(repo: Path, args: list[str]) -> list[str]:
    out = run(["git", *args], cwd=repo, timeout=600).stdout
    return [line for line in out.splitlines() if line]


def read_proc_stat(pid: int) -> tuple[int, int, int] | None:
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    end = text.rfind(")")
    if end == -1:
        return None
    parts = text[end + 2 :].split()
    try:
        ppid = int(parts[1])
        utime = int(parts[11])
        stime = int(parts[12])
        return ppid, utime, stime
    except (IndexError, ValueError):
        return None


def process_tree(root_pid: int) -> list[int]:
    children: dict[int, list[int]] = {}
    live: set[int] = set()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        stat = read_proc_stat(pid)
        if stat is None:
            continue
        ppid, _, _ = stat
        live.add(pid)
        children.setdefault(ppid, []).append(pid)
    if root_pid not in live:
        return []
    out: list[int] = []
    stack = [root_pid]
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        out.append(pid)
        stack.extend(children.get(pid, []))
    return out


def tree_ticks_and_rss(root_pid: int) -> tuple[int, int, list[int]]:
    ticks = 0
    rss = 0
    pids = process_tree(root_pid)
    page_size = os.sysconf("SC_PAGE_SIZE")
    for pid in pids:
        stat = read_proc_stat(pid)
        if stat is not None:
            _, utime, stime = stat
            ticks += utime + stime
        try:
            fields = Path(f"/proc/{pid}/statm").read_text().split()
            rss += int(fields[1]) * page_size
        except (OSError, IndexError, ValueError):
            pass
    return ticks, rss, pids


class BenchmarkState:
    def __init__(self, semantic_enabled: bool, search_enabled: bool = True) -> None:
        self.lock = threading.Lock()
        self.t0: float | None = None
        self.search_enabled = search_enabled
        self.semantic_enabled = semantic_enabled
        self.search_status = "loading" if search_enabled else "disabled"
        self.semantic_status = "loading" if semantic_enabled else "disabled"
        self.semantic_stage: str | None = None
        self.search_ready_offset: float | None = None
        self.semantic_terminal_offset: float | None = None
        self.status_bar_ready_offset: float | None = None
        self.configure_warning_source_files: int | None = None
        self.configure_response: dict[str, Any] | None = None
        self.latest_status: dict[str, Any] = {}
        self.search_log: dict[str, Any] | None = None
        self.semantic_collect: dict[str, Any] | None = None
        self.semantic_embed: dict[str, Any] | None = None
        self.semantic_built: dict[str, Any] | None = None
        self.semantic_failure: str | None = None
        self.tier2_reason: str | None = None
        self.tier2_scheduled_offset: float | None = None
        self.tier2_active: str | None = None
        self.callgraph_active = False
        self.tier2_categories: dict[str, dict[str, Any]] = {category: {} for category in TIER2_CATEGORIES}
        self.callgraph_snapshots: list[dict[str, Any]] = []
        self.events: list[dict[str, Any]] = []
        self.settled_offset: float | None = None
        self.all_done_offset: float | None = None

    def set_t0(self) -> None:
        with self.lock:
            self.t0 = time.monotonic()
            self.events.append({"offset_s": 0.0, "event": "configure_sent"})

    def offset(self) -> float | None:
        with self.lock:
            return self._offset_unlocked()

    def _offset_unlocked(self) -> float | None:
        if self.t0 is None:
            return None
        return time.monotonic() - self.t0

    def _event(self, offset: float | None, event: str, **fields: Any) -> None:
        item: dict[str, Any] = {"offset_s": round(offset, 3) if offset is not None else None, "event": event}
        item.update(fields)
        self.events.append(item)

    def update_configure_response(self, response: dict[str, Any]) -> None:
        with self.lock:
            self.configure_response = response
            self._event(self._offset_unlocked(), "configure_response", source_file_count=response.get("source_file_count"))

    def update_push_frame(self, frame: dict[str, Any]) -> None:
        offset = self.offset()
        frame_type = frame.get("type")
        if frame_type == "status_changed":
            snapshot = frame.get("snapshot")
            if isinstance(snapshot, dict):
                self.update_status(snapshot, offset, source="push")
        elif frame_type == "configure_warnings":
            with self.lock:
                self.configure_warning_source_files = int(frame.get("source_file_count") or 0)
                self._event(
                    offset,
                    "configure_warnings",
                    source_file_count=self.configure_warning_source_files,
                )

    def update_response(self, response: dict[str, Any]) -> None:
        if "search_index" in response or "semantic_index" in response or "status_bar" in response:
            self.update_status(response, self.offset(), source="status")

    def update_status(self, snapshot: dict[str, Any], offset: float | None, source: str) -> None:
        with self.lock:
            self.latest_status = snapshot
            search = snapshot.get("search_index") if isinstance(snapshot.get("search_index"), dict) else {}
            search_status = search.get("status")
            if isinstance(search_status, str) and search_status != self.search_status:
                self.search_status = search_status
                self._event(offset, "search_status", status=search_status, source=source)
            if search_status == "ready" and self.search_ready_offset is None:
                self.search_ready_offset = offset

            semantic = snapshot.get("semantic_index") if isinstance(snapshot.get("semantic_index"), dict) else {}
            semantic_status = semantic.get("status") or semantic.get("state")
            semantic_stage = semantic.get("stage")
            if isinstance(semantic_status, str) and semantic_status != self.semantic_status:
                self.semantic_status = semantic_status
                self._event(offset, "semantic_status", status=semantic_status, stage=semantic_stage, source=source)
            if isinstance(semantic_stage, str) and semantic_stage != self.semantic_stage:
                self.semantic_stage = semantic_stage
                self._event(offset, "semantic_stage", status=semantic_status, stage=semantic_stage, source=source)
            if semantic_status in TERMINAL_INDEX_STATES and self.semantic_terminal_offset is None:
                self.semantic_terminal_offset = offset

            status_bar = snapshot.get("status_bar")
            if isinstance(status_bar, dict):
                has_counts = all(status_bar.get(key) is not None for key in TIER2_CATEGORIES)
                if has_counts and not bool(status_bar.get("tier2_stale")) and self.status_bar_ready_offset is None:
                    self.status_bar_ready_offset = offset
                    self._event(offset, "tier2_status_bar_ready", source=source)

            if self.all_known_done_unlocked() and self.all_done_offset is None:
                self.all_done_offset = offset
                self._event(offset, "all_known_phases_done")

    def update_log_line(self, line: str) -> None:
        offset = self.offset()
        with self.lock:
            if match := re.search(r"search index cold build: (\d+) files, (\d+) trigrams, (\d+) ms", line):
                files, trigrams, ms = map(int, match.groups())
                self.search_log = {
                    "files": files,
                    "trigrams": trigrams,
                    "ms": ms,
                    "start_offset_s": round(max(0.0, (offset or 0.0) - ms / 1000.0), 3),
                    "end_offset_s": round(offset or 0.0, 3),
                }
                self._event(offset, "search_index_cold_build", files=files, trigrams=trigrams, ms=ms)
            elif match := re.search(r"semantic collect: (\d+) chunks from (\d+) files in (\d+) ms", line):
                chunks, files, ms = map(int, match.groups())
                self.semantic_collect = {
                    "chunks": chunks,
                    "files": files,
                    "ms": ms,
                    "start_offset_s": round(max(0.0, (offset or 0.0) - ms / 1000.0), 3),
                    "end_offset_s": round(offset or 0.0, 3),
                }
                self._event(offset, "semantic_collect", chunks=chunks, files=files, ms=ms)
            elif match := re.search(r"semantic embed: (\d+) chunks in (\d+) batches, (\d+) ms \((\d+) chunks/s\)", line):
                chunks, batches, ms, rate = map(int, match.groups())
                self.semantic_embed = {
                    "chunks": chunks,
                    "batches": batches,
                    "ms": ms,
                    "rate_chunks_per_s": rate,
                    "start_offset_s": round(max(0.0, (offset or 0.0) - ms / 1000.0), 3),
                    "end_offset_s": round(offset or 0.0, 3),
                }
                self._event(offset, "semantic_embed", chunks=chunks, batches=batches, ms=ms, rate_chunks_per_s=rate)
            elif match := re.search(r"built semantic index: (\d+) files, (\d+) entries", line):
                files, entries = map(int, match.groups())
                self.semantic_built = {"files": files, "entries": entries, "offset_s": round(offset or 0.0, 3)}
                self._event(offset, "semantic_built", files=files, entries=entries)
            elif match := re.search(r"failed to build semantic index: (.*)", line):
                self.semantic_failure = match.group(1)
                self._event(offset, "semantic_failed", message=self.semantic_failure)
            elif match := re.search(r"tier2 refresh scheduled: reason=([^,]+), categories=\[(.*)\]", line):
                self.tier2_reason = match.group(1)
                self.tier2_scheduled_offset = offset
                self._event(offset, "tier2_scheduled", reason=self.tier2_reason, categories=match.group(2))
            elif match := re.search(r"settle bench: tier2_category_start category=(\w+) job_id=(\d+) files=(\d+)", line):
                category, job_id, files = match.groups()
                self.tier2_active = category
                record = self.tier2_categories.setdefault(category, {})
                record.update({"job_id": int(job_id), "files": int(files), "start_offset_s": round(offset or 0.0, 3)})
                self._event(offset, "tier2_category_start", category=category, job_id=int(job_id), files=int(files))
            elif match := re.search(
                r"settle bench: tier2_category_end category=(\w+) job_id=(\d+) status=(\w+) total_ms=(\d+)(?: scanned_files=(\d+) contributions=(\d+) count=(\d+)| error=(.*))",
                line,
            ):
                category, job_id, status, total_ms, scanned, contributions, count, error = match.groups()
                record = self.tier2_categories.setdefault(category, {})
                record.update(
                    {
                        "job_id": int(job_id),
                        "status": status,
                        "total_ms": int(total_ms),
                        "end_offset_s": round(offset or 0.0, 3),
                    }
                )
                if scanned is not None:
                    record.update(
                        {
                            "scanned_files": int(scanned),
                            "contributions": int(contributions),
                            "count": int(count),
                        }
                    )
                if error:
                    record["error"] = error
                if self.tier2_active == category:
                    self.tier2_active = None
                self._event(offset, "tier2_category_end", category=category, status=status, total_ms=int(total_ms))
            elif match := re.search(r"settle bench: tier2_callgraph_snapshot_start files=(\d+)", line):
                self.callgraph_active = True
                self.callgraph_snapshots.append({"files": int(match.group(1)), "start_offset_s": round(offset or 0.0, 3)})
                self._event(offset, "tier2_callgraph_snapshot_start", files=int(match.group(1)))
            elif match := re.search(
                r"settle bench: tier2_callgraph_snapshot_end files=(\d+) built_files=(\d+) exported_symbols=(\d+) outbound_calls=(\d+) entry_points=(\d+) ms=(\d+)",
                line,
            ):
                files, built_files, exported_symbols, outbound_calls, entry_points, ms = map(int, match.groups())
                target = self.callgraph_snapshots[-1] if self.callgraph_snapshots else {}
                target.update(
                    {
                        "files": files,
                        "built_files": built_files,
                        "exported_symbols": exported_symbols,
                        "outbound_calls": outbound_calls,
                        "entry_points": entry_points,
                        "ms": ms,
                        "end_offset_s": round(offset or 0.0, 3),
                    }
                )
                if not self.callgraph_snapshots:
                    self.callgraph_snapshots.append(target)
                self.callgraph_active = False
                self._event(offset, "tier2_callgraph_snapshot_end", files=files, built_files=built_files, ms=ms)

            if self.all_known_done_unlocked() and self.all_done_offset is None:
                self.all_done_offset = offset
                self._event(offset, "all_known_phases_done")

    def all_known_done(self) -> bool:
        with self.lock:
            return self.all_known_done_unlocked()

    def all_known_done_unlocked(self) -> bool:
        search_done = (not self.search_enabled) or self.search_status in TERMINAL_INDEX_STATES or self.search_ready_offset is not None
        semantic_done = (not self.semantic_enabled) or self.semantic_status in TERMINAL_INDEX_STATES
        tier2_done = all(self.tier2_categories.get(category, {}).get("end_offset_s") is not None for category in TIER2_CATEGORIES)
        return bool(search_done and semantic_done and tier2_done)

    def current_phase(self) -> str:
        with self.lock:
            search_loading = self.search_enabled and self.search_status not in TERMINAL_INDEX_STATES
            semantic_loading = self.semantic_enabled and self.semantic_status not in TERMINAL_INDEX_STATES
            if self.callgraph_active:
                return "tier2:callgraph_snapshot"
            if self.tier2_active:
                return f"tier2:{self.tier2_active}"
            if search_loading and semantic_loading:
                return f"search+semantic:{self.semantic_stage or self.semantic_status}"
            if search_loading:
                return "search_index"
            if semantic_loading:
                return f"semantic:{self.semantic_stage or self.semantic_status}"
            return "idle_or_status"

    def snapshot(self) -> dict[str, Any]:
        with self.lock:
            return {
                "search_status": self.search_status,
                "semantic_status": self.semantic_status,
                "semantic_stage": self.semantic_stage,
                "search_ready_offset_s": self.search_ready_offset,
                "semantic_terminal_offset_s": self.semantic_terminal_offset,
                "status_bar_ready_offset_s": self.status_bar_ready_offset,
                "configure_warning_source_files": self.configure_warning_source_files,
                "configure_response": self.configure_response,
                "latest_status": self.latest_status,
                "search_log": self.search_log,
                "semantic_collect": self.semantic_collect,
                "semantic_embed": self.semantic_embed,
                "semantic_built": self.semantic_built,
                "semantic_failure": self.semantic_failure,
                "tier2_reason": self.tier2_reason,
                "tier2_scheduled_offset_s": self.tier2_scheduled_offset,
                "tier2_categories": self.tier2_categories,
                "callgraph_snapshots": self.callgraph_snapshots,
                "events": self.events,
                "all_done_offset_s": self.all_done_offset,
                "settled_offset_s": self.settled_offset,
            }


class AftClient:
    def __init__(self, aft_binary: str, state: BenchmarkState, out_dir: Path, env: dict[str, str]) -> None:
        self.state = state
        self.out_dir = out_dir
        self.response_lock = threading.Lock()
        self.responses: dict[str, dict[str, Any]] = {}
        self.next_id = 0
        self.proc = subprocess.Popen(
            [aft_binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        self.stdout_file = (out_dir / "aft-stdout.ndjson").open("w", encoding="utf-8")
        self.log_file = (out_dir / "aft.log").open("w", encoding="utf-8")
        self._stdout_thread = threading.Thread(target=self._read_stdout, name="aft-stdout", daemon=True)
        self._stderr_thread = threading.Thread(target=self._read_stderr, name="aft-stderr", daemon=True)
        self._stdout_thread.start()
        self._stderr_thread.start()

    @property
    def pid(self) -> int:
        return self.proc.pid

    def _read_stdout(self) -> None:
        assert self.proc.stdout is not None
        for raw in self.proc.stdout:
            line = raw.strip()
            if not line:
                continue
            self.stdout_file.write(line + "\n")
            self.stdout_file.flush()
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict) and obj.get("type"):
                self.state.update_push_frame(obj)
            if isinstance(obj, dict) and "id" in obj:
                with self.response_lock:
                    self.responses[str(obj["id"])] = obj
                self.state.update_response(obj)

    def _read_stderr(self) -> None:
        assert self.proc.stderr is not None
        for raw in self.proc.stderr:
            line = raw.rstrip("\n")
            offset = self.state.offset()
            prefix = "pre" if offset is None else f"+{offset:.3f}s"
            self.log_file.write(f"{prefix} {line}\n")
            self.log_file.flush()
            self.state.update_log_line(line)

    def call(self, command: str, params: dict[str, Any] | None = None, timeout: float = 30.0) -> dict[str, Any]:
        self.next_id += 1
        req_id = f"bench-{self.next_id}"
        request: dict[str, Any] = {"id": req_id, "command": command, "session_id": "settle-bench"}
        if params:
            request.update(params)
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"aft exited with code {self.proc.returncode} while waiting for {command}")
            with self.response_lock:
                response = self.responses.pop(req_id, None)
            if response is not None:
                if not response.get("success", False):
                    raise RuntimeError(f"{command} failed: {json.dumps(response, sort_keys=True)}")
                return response
            time.sleep(0.05)
        raise TimeoutError(f"timed out waiting for {command} response after {timeout}s")

    def close(self) -> None:
        try:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=5)
        finally:
            self.stdout_file.close()
            self.log_file.close()


def start_sampler(pid: int, state: BenchmarkState, samples: list[dict[str, Any]], stop: threading.Event, interval: float) -> threading.Thread:
    ticks_per_second = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
    previous_ticks, _, _ = tree_ticks_and_rss(pid)
    previous_time = time.monotonic()

    def sample_loop() -> None:
        nonlocal previous_ticks, previous_time
        while not stop.wait(interval):
            now = time.monotonic()
            ticks, rss_bytes, pids = tree_ticks_and_rss(pid)
            elapsed = max(0.001, now - previous_time)
            cpu_percent = max(0.0, (ticks - previous_ticks) / ticks_per_second / elapsed * 100.0)
            offset = state.offset()
            samples.append(
                {
                    "offset_s": round(offset or 0.0, 3),
                    "cpu_percent_one_core": round(cpu_percent, 2),
                    "rss_mb": round(rss_bytes / (1024 * 1024), 2),
                    "pid_count": len(pids),
                    "phase": state.current_phase(),
                }
            )
            previous_ticks, previous_time = ticks, now

    thread = threading.Thread(target=sample_loop, name="proc-sampler", daemon=True)
    thread.start()
    return thread


def write_samples(path: Path, samples: list[dict[str, Any]]) -> None:
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=["offset_s", "cpu_percent_one_core", "rss_mb", "pid_count", "phase"])
        writer.writeheader()
        writer.writerows(samples)


def peak_sample(samples: list[dict[str, Any]], key: str, predicate=lambda sample: True) -> dict[str, Any] | None:
    filtered = [sample for sample in samples if predicate(sample)]
    if not filtered:
        return None
    return max(filtered, key=lambda sample: float(sample[key]))


def build_summary(
    *,
    url: str,
    requested_ref: str | None,
    repo: Path,
    out_dir: Path,
    semantic_enabled: bool,
    state: BenchmarkState,
    samples: list[dict[str, Any]],
    tracked_files: int,
    commit: str,
    started_at: str,
    ended_at: str,
    timeout: bool,
) -> dict[str, Any]:
    snapshot = state.snapshot()
    latest_status = snapshot.get("latest_status") or {}
    search_index = latest_status.get("search_index") if isinstance(latest_status.get("search_index"), dict) else {}
    semantic_index = latest_status.get("semantic_index") if isinstance(latest_status.get("semantic_index"), dict) else {}
    source_file_count = snapshot.get("configure_warning_source_files")
    if source_file_count is None and isinstance(search_index, dict):
        source_file_count = search_index.get("files")

    peak_cpu = peak_sample(samples, "cpu_percent_one_core")
    peak_rss = peak_sample(samples, "rss_mb")
    peak_embed_rss = peak_sample(samples, "rss_mb", lambda s: "semantic:embedding_symbols" in str(s.get("phase")) or "search+semantic:embedding_symbols" in str(s.get("phase")))

    tier2_categories = snapshot.get("tier2_categories") or {}
    tier2_end_offsets = [
        tier2_categories.get(category, {}).get("end_offset_s")
        for category in TIER2_CATEGORIES
    ]
    tier2_complete = max(tier2_end_offsets) if all(offset is not None for offset in tier2_end_offsets) else None
    callgraph_snapshots = snapshot.get("callgraph_snapshots") or []
    first_callgraph = callgraph_snapshots[0] if callgraph_snapshots else None

    return {
        "repo": {
            "url": url,
            "requested_ref": requested_ref,
            "commit": commit,
            "tracked_files": tracked_files,
            "aft_source_files": source_file_count,
            "search_index_files": search_index.get("files") if isinstance(search_index, dict) else None,
        },
        "config": {
            "search_index": True,
            "semantic_search": semantic_enabled,
            "semantic_backend": "fastembed" if semantic_enabled else None,
            "tier2_trigger_path": "watcher_configure_warm",
            "settle_cpu_threshold_percent_one_core": float(os.environ.get("AFT_SETTLE_CPU_THRESHOLD", "5")),
            "settle_idle_seconds": float(os.environ.get("AFT_SETTLE_IDLE_SECS", "30")),
        },
        "environment": {
            "started_at": started_at,
            "ended_at": ended_at,
            "container_uname": command_output(["uname", "-a"]),
            "container_arch": command_output(["uname", "-m"]),
            "nproc": command_output(["nproc"]),
            "aft_version": command_output(["aft", "--version"]),
            "ort_dylib_path": os.environ.get("ORT_DYLIB_PATH"),
            "fastembed_cache_dir": os.environ.get("FASTEMBED_CACHE_DIR"),
            "storage_dir": latest_status.get("storage_dir") if isinstance(latest_status, dict) else None,
        },
        "metrics": {
            "timeout": timeout,
            "time_to_search_index_ready_s": snapshot.get("search_ready_offset_s"),
            "time_to_semantic_terminal_s": snapshot.get("semantic_terminal_offset_s"),
            "semantic_status": snapshot.get("semantic_status"),
            "semantic_entries": semantic_index.get("entries") if isinstance(semantic_index, dict) else None,
            "semantic_collect": snapshot.get("semantic_collect"),
            "semantic_embed": snapshot.get("semantic_embed"),
            "semantic_built": snapshot.get("semantic_built"),
            "semantic_failure": snapshot.get("semantic_failure"),
            "time_to_tier2_complete_s": tier2_complete,
            "tier2_reason": snapshot.get("tier2_reason"),
            "tier2_scheduled_offset_s": snapshot.get("tier2_scheduled_offset_s"),
            "tier2_categories": tier2_categories,
            "dead_code_callgraph_snapshot": first_callgraph,
            "all_known_done_offset_s": snapshot.get("all_done_offset_s"),
            "total_settle_time_s": snapshot.get("settled_offset_s"),
            "last_sample": samples[-1] if samples else None,
            "cutoff_phase": (samples[-1].get("phase") if samples else None) if timeout else None,
            "cutoff_cpu_percent_one_core": (samples[-1].get("cpu_percent_one_core") if samples else None) if timeout else None,
            "cutoff_rss_mb": (samples[-1].get("rss_mb") if samples else None) if timeout else None,
            "peak_cpu_percent_one_core": peak_cpu,
            "peak_rss_mb": peak_rss,
            "peak_rss_during_semantic_embed_mb": peak_embed_rss,
        },
        "timeline": snapshot.get("events"),
        "artifacts": {
            "out_dir": str(out_dir),
            "summary_json": str(out_dir / "summary.json"),
            "samples_csv": str(out_dir / "samples.csv"),
            "aft_log": str(out_dir / "aft.log"),
            "aft_stdout_ndjson": str(out_dir / "aft-stdout.ndjson"),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("github_url")
    parser.add_argument("ref", nargs="?")
    args = parser.parse_args()

    semantic_env = os.environ.get("AFT_SETTLE_SEMANTIC", "on").strip().lower()
    if semantic_env not in {"on", "off", "true", "false", "1", "0"}:
        raise SystemExit("AFT_SETTLE_SEMANTIC must be on/off")
    semantic_enabled = semantic_env in {"on", "true", "1"}
    timeout_secs = float(os.environ.get("AFT_SETTLE_TIMEOUT_SECS", "1800"))
    idle_secs = float(os.environ.get("AFT_SETTLE_IDLE_SECS", "30"))
    cpu_threshold = float(os.environ.get("AFT_SETTLE_CPU_THRESHOLD", "5"))
    status_interval = float(os.environ.get("AFT_SETTLE_STATUS_INTERVAL", "5"))
    sample_interval = float(os.environ.get("AFT_SETTLE_SAMPLE_INTERVAL", "5"))
    progress_interval = float(os.environ.get("AFT_SETTLE_PROGRESS_INTERVAL", "30"))

    work_root = Path("/work")
    clone_path = work_root / "repo"
    work_root.mkdir(parents=True, exist_ok=True)

    print(f"==> Cloning {args.github_url} {args.ref or ''}".rstrip(), flush=True)
    clone_repo(args.github_url, args.ref, clone_path)
    commit = command_output(["git", "rev-parse", "HEAD"], cwd=clone_path)
    tracked_files = len(git_lines(clone_path, ["ls-files"]))
    slug = repo_slug(args.github_url)
    run_id = f"{slug}--{commit[:12]}--semantic-{'on' if semantic_enabled else 'off'}"
    out_dir = Path("/results") / run_id
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    storage_dir = Path("/cache/aft-storage") / run_id
    if os.environ.get("AFT_SETTLE_CLEAR_STORAGE", "1") == "1" and storage_dir.exists():
        shutil.rmtree(storage_dir)
    storage_dir.mkdir(parents=True, exist_ok=True)
    Path(os.environ.get("FASTEMBED_CACHE_DIR", "/cache/fastembed")).mkdir(parents=True, exist_ok=True)

    print(f"==> Repo commit: {commit}", flush=True)
    print(f"==> Tracked files: {tracked_files}", flush=True)
    print(f"==> Results: {out_dir}", flush=True)

    state = BenchmarkState(semantic_enabled=semantic_enabled)
    env = os.environ.copy()
    env.update(
        {
            "AFT_SETTLE_BENCH_LOG": "1",
            "AFT_STORAGE_DIR": str(storage_dir),
            "RUST_LOG": env.get("RUST_LOG", "info"),
        }
    )
    client = AftClient("aft", state, out_dir, env)
    samples: list[dict[str, Any]] = []
    stop_sampler = threading.Event()
    sampler_thread = start_sampler(client.pid, state, samples, stop_sampler, interval=sample_interval)
    started_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    timeout = False
    idle_start: float | None = None
    last_progress = 0.0

    try:
        state.set_t0()
        configure = client.call(
            "configure",
            {
                "project_root": str(clone_path.resolve()),
                "harness": "opencode",
                "storage_dir": str(storage_dir.resolve()),
                "search_index": True,
                "semantic_search": semantic_enabled,
                "semantic": {
                    "backend": "fastembed",
                    "model": "all-MiniLM-L6-v2",
                    "max_batch_size": 64,
                    "max_files": 20000,
                },
            },
            timeout=60,
        )
        state.update_configure_response(configure)

        deadline = time.monotonic() + timeout_secs
        while time.monotonic() < deadline:
            try:
                client.call("status", {}, timeout=max(5.0, status_interval * 5))
            except TimeoutError as error:
                print(f"WARN: status timed out: {error}", flush=True)
            except RuntimeError:
                raise

            last_sample = samples[-1] if samples else {"cpu_percent_one_core": 999.0, "rss_mb": 0.0, "phase": "unknown"}
            now = time.monotonic()
            current_offset = state.offset() or 0.0
            if current_offset - last_progress >= progress_interval:
                snap = state.snapshot()
                tier2_complete = all(
                    (snap.get("tier2_categories") or {}).get(category, {}).get("end_offset_s") is not None
                    for category in TIER2_CATEGORIES
                )
                print(
                    "PROGRESS "
                    f"t={current_offset:.1f}s phase={last_sample.get('phase')} "
                    f"search={snap.get('search_ready_offset_s')} "
                    f"semantic={snap.get('semantic_terminal_offset_s')} "
                    f"tier2_done={tier2_complete} "
                    f"cpu={last_sample.get('cpu_percent_one_core')}% rss={last_sample.get('rss_mb')}MB",
                    flush=True,
                )
                last_progress = current_offset
            if state.all_known_done():
                cpu = float(last_sample.get("cpu_percent_one_core", 999.0))
                if cpu < cpu_threshold:
                    if idle_start is None:
                        idle_start = now
                    if now - idle_start >= idle_secs:
                        with state.lock:
                            state.settled_offset = state._offset_unlocked()
                            state._event(state.settled_offset, "settled", idle_seconds=idle_secs, cpu_threshold=cpu_threshold)
                        break
                else:
                    idle_start = None
            time.sleep(status_interval)
        else:
            timeout = True
            print(f"ERROR: benchmark timed out after {timeout_secs}s", flush=True)
    finally:
        try:
            client.call("status", {}, timeout=5)
        except Exception:
            pass
        stop_sampler.set()
        sampler_thread.join(timeout=3)
        client.close()

    ended_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    write_samples(out_dir / "samples.csv", samples)
    summary = build_summary(
        url=args.github_url,
        requested_ref=args.ref,
        repo=clone_path,
        out_dir=out_dir,
        semantic_enabled=semantic_enabled,
        state=state,
        samples=samples,
        tracked_files=tracked_files,
        commit=commit,
        started_at=started_at,
        ended_at=ended_at,
        timeout=timeout,
    )
    (out_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    print(json.dumps({"summary_json": str(out_dir / "summary.json"), "metrics": summary["metrics"]}, indent=2), flush=True)
    return 124 if timeout else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        os.kill(os.getpid(), signal.SIGINT)
        raise
