#!/usr/bin/env python3
"""Prove that every Rust test lands in exactly one split-gate bucket.

This script captures the current workspace inventory from `cargo test --workspace
-- --list`, then compares it against the split buckets used by
`scripts/rust-test-gate.sh`:

* `cargo test --workspace --lib --bins`
* `cargo nextest list --workspace -E 'kind(test) - binary(=watcher_integration)'`
* `cargo test -p agent-file-tools --test watcher_integration`

Doctests are checked as a separate cargo bucket only when the baseline inventory
contains doctest cases.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HASH_SUFFIX_RE = re.compile(r"-[0-9a-f]+(?:\.exe)?$")
RUNNING_RE = re.compile(r"^\s*Running (.+) \((.+)\)$")
COUNT_RE = re.compile(r"^\d+ tests?, \d+ benchmarks$")
TEST_LINE_RE = re.compile(r"^(.*): test$")


@dataclass(frozen=True)
class CargoEntry:
    kind: str
    suite: str
    source: str
    name: str

    @property
    def key(self) -> str:
        if self.kind == "test":
            return f"test|{self.suite}|{self.name}"
        return f"{self.kind}|{self.suite}|{self.source}|{self.name}"


@dataclass(frozen=True)
class NextestEntry:
    suite: str
    name: str

    @property
    def key(self) -> str:
        return f"test|{self.suite}|{self.name}"


def run_command(command: list[str]) -> str:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
        env=os.environ.copy(),
    )
    output = completed.stdout or ""
    filtered_output = "\n".join(
        line
        for line in output.splitlines()
        if not line.startswith("    Blocking waiting for file lock on package cache")
    )
    if output.endswith("\n"):
        filtered_output += "\n"
    if completed.returncode != 0:
        sys.stderr.write(f"command failed ({completed.returncode}): {' '.join(command)}\n")
        sys.stderr.write(filtered_output)
        sys.exit(completed.returncode)
    return filtered_output

def suite_name_from_artifact(artifact_path: str) -> str:
    artifact = Path(artifact_path).name
    return HASH_SUFFIX_RE.sub("", artifact)


def parse_cargo_list(output: str) -> list[CargoEntry]:
    current_kind: str | None = None
    current_suite: str | None = None
    current_source: str | None = None
    entries: list[CargoEntry] = []

    for raw_line in output.splitlines():
        line = raw_line.rstrip("\n")
        stripped = line.strip()
        if not stripped or stripped.startswith("Finished `"):
            continue

        if stripped.startswith("Doc-tests "):
            current_kind = "doc"
            current_suite = stripped.removeprefix("Doc-tests ")
            current_source = current_suite
            continue

        running_match = RUNNING_RE.match(line)
        if running_match:
            source = running_match.group(1)
            artifact_path = running_match.group(2)
            suite = suite_name_from_artifact(artifact_path)
            kind = "test"
            if source.startswith("unittests "):
                unit_source = source.removeprefix("unittests ")
                kind = "lib" if unit_source == "src/lib.rs" else "bin"
                source = unit_source
            current_kind = kind
            current_suite = suite
            current_source = source
            continue

        if COUNT_RE.match(stripped):
            continue

        test_match = TEST_LINE_RE.match(stripped)
        if test_match and current_kind and current_suite and current_source:
            entries.append(
                CargoEntry(
                    kind=current_kind,
                    suite=current_suite,
                    source=current_source,
                    name=test_match.group(1),
                )
            )
            continue

        raise RuntimeError(f"unrecognized cargo list line: {line}")

    return entries


def parse_nextest_list(output: str) -> list[NextestEntry]:
    payload = json.loads(output)
    entries: list[NextestEntry] = []
    for suite_data in payload.get("rust-suites", {}).values():
        if suite_data.get("kind") != "test":
            continue
        suite = suite_data["binary-name"]
        for name, testcase in suite_data.get("testcases", {}).items():
            if testcase.get("kind") != "test":
                continue
            entries.append(NextestEntry(suite=suite, name=name))
    return entries


def summarize(counter: Counter[str]) -> list[str]:
    return [f"{bucket}={counter[bucket]}" for bucket in sorted(counter)]


baseline_output = run_command(["cargo", "test", "--workspace", "--", "--list"])
baseline_entries = parse_cargo_list(baseline_output)

unit_bin_output = run_command([
    "cargo",
    "test",
    "--workspace",
    "--lib",
    "--bins",
    "--",
    "--list",
])
unit_bin_entries = [
    entry for entry in parse_cargo_list(unit_bin_output) if entry.kind in {"lib", "bin"}
]

doc_baseline_entries = [entry for entry in baseline_entries if entry.kind == "doc"]
doc_entries: list[CargoEntry] = []
if doc_baseline_entries:
    doc_output = run_command(["cargo", "test", "--workspace", "--doc", "--", "--list"])
    doc_entries = [entry for entry in parse_cargo_list(doc_output) if entry.kind == "doc"]

nextest_output = run_command([
    "cargo",
    "nextest",
    "list",
    "--workspace",
    "-E",
    "kind(test) - binary(=watcher_integration)",
    "-T",
    "json",
    "--cargo-quiet",
])
nextest_entries = parse_nextest_list(nextest_output)

watcher_output = run_command([
    "cargo",
    "test",
    "-p",
    "agent-file-tools",
    "--test",
    "watcher_integration",
    "--",
    "--list",
])
watcher_entries = [entry for entry in parse_cargo_list(watcher_output) if entry.kind == "test"]

baseline_by_bucket = Counter(entry.kind for entry in baseline_entries)
coverage: dict[str, list[str]] = {}

for entry in unit_bin_entries:
    coverage.setdefault(entry.key, []).append("cargo-lib-bin")
for entry in doc_entries:
    coverage.setdefault(entry.key, []).append("cargo-doc")
for entry in nextest_entries:
    coverage.setdefault(entry.key, []).append("nextest-integration")
for entry in watcher_entries:
    coverage.setdefault(entry.key, []).append("cargo-watcher")

baseline_keys = {entry.key for entry in baseline_entries}
covered_keys = set(coverage)
uncovered = sorted(key for key in baseline_keys if key not in covered_keys)
double_covered = sorted(
    (key, buckets) for key, buckets in coverage.items() if key in baseline_keys and len(buckets) > 1
)
extra_keys = sorted(key for key in covered_keys if key not in baseline_keys)
watcher_in_nextest = sorted(
    entry.key for entry in nextest_entries if entry.suite == "watcher_integration"
)

print("Baseline counts:", ", ".join(summarize(baseline_by_bucket)))
print(
    "Split counts:",
    ", ".join(
        [
            f"cargo-lib-bin={len(unit_bin_entries)}",
            f"cargo-doc={len(doc_entries)}",
            f"nextest-integration={len(nextest_entries)}",
            f"cargo-watcher={len(watcher_entries)}",
        ]
    ),
)
print(f"Watcher tests in nextest inventory: {len(watcher_in_nextest)}")
print(f"Doctest cases in baseline inventory: {len(doc_baseline_entries)}")

if uncovered or double_covered or extra_keys or watcher_in_nextest:
    print("\nInventory proof failed.")
    if uncovered:
        print(f"  uncovered ({len(uncovered)}):")
        for key in uncovered[:20]:
            print(f"    {key}")
    if double_covered:
        print(f"  double-covered ({len(double_covered)}):")
        for key, buckets in double_covered[:20]:
            print(f"    {key} -> {', '.join(buckets)}")
    if extra_keys:
        print(f"  extra split-only entries ({len(extra_keys)}):")
        for key in extra_keys[:20]:
            print(f"    {key}")
    if watcher_in_nextest:
        print(f"  watcher leaked into nextest ({len(watcher_in_nextest)}):")
        for key in watcher_in_nextest[:20]:
            print(f"    {key}")
    sys.exit(1)

print("\nInventory proof passed: every baseline test is covered exactly once.")
