import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

import { dirSize } from "./fs-util.js";

const SQLITE_SUFFIXES = [".sqlite-wal", ".sqlite-shm", ".sqlite-journal", ".sqlite"];

export interface LegacyHarnessDuplication {
  harness: string;
  partitions: number;
  bytes: number;
}

export interface LegacyPartitionDuplicationSummary {
  totalPartitions: number;
  totalBytes: number;
  byHarness: LegacyHarnessDuplication[];
}

export function summarizeLegacyPartitionDuplication(
  storageRoot: string,
): LegacyPartitionDuplicationSummary {
  if (!existsSync(storageRoot)) {
    return { totalPartitions: 0, totalBytes: 0, byHarness: [] };
  }

  const byHarness: LegacyHarnessDuplication[] = [];
  for (const harness of safeReadDir(storageRoot)) {
    const harnessPath = join(storageRoot, harness);
    if (!isDirectory(harnessPath)) continue;

    const partitions = new Map<string, number>();
    collectCallgraphPartitions(join(harnessPath, "callgraph"), partitions);
    collectInspectPartitions(join(harnessPath, "inspect"), partitions);
    if (partitions.size === 0) continue;

    let bytes = 0;
    for (const size of partitions.values()) bytes += size;
    byHarness.push({ harness, partitions: partitions.size, bytes });
  }

  byHarness.sort((left, right) => left.harness.localeCompare(right.harness));
  return {
    totalPartitions: byHarness.reduce((sum, item) => sum + item.partitions, 0),
    totalBytes: byHarness.reduce((sum, item) => sum + item.bytes, 0),
    byHarness,
  };
}

function collectCallgraphPartitions(domainPath: string, partitions: Map<string, number>): void {
  if (!isDirectory(domainPath)) return;
  for (const name of safeReadDir(domainPath)) {
    const path = join(domainPath, name);
    if (isDirectory(path)) {
      if (!looksLikePartitionKey(name)) continue;
      addPartitionBytes(partitions, `callgraph:${name}`, dirSize(path));
      continue;
    }

    const key = callgraphPartitionKeyFromName(name);
    if (!key) continue;
    addPartitionBytes(partitions, `callgraph:${key}`, safeFileSize(path));
  }
}

function collectInspectPartitions(domainPath: string, partitions: Map<string, number>): void {
  if (!isDirectory(domainPath)) return;
  for (const name of safeReadDir(domainPath)) {
    const path = join(domainPath, name);
    if (isDirectory(path)) {
      if (!looksLikePartitionKey(name)) continue;
      addPartitionBytes(partitions, `inspect:${name}`, dirSize(path));
      continue;
    }

    const key = inspectPartitionKeyFromName(name);
    if (!key) continue;
    addPartitionBytes(partitions, `inspect:${key}`, safeFileSize(path));
  }
}

function addPartitionBytes(partitions: Map<string, number>, key: string, bytes: number): void {
  partitions.set(key, (partitions.get(key) ?? 0) + bytes);
}

function callgraphPartitionKeyFromName(name: string): string | null {
  if (name.includes(".tmp.")) return null;
  if (name.endsWith(".current")) {
    const key = name.slice(0, -".current".length);
    return looksLikePartitionKey(key) ? key : null;
  }

  const base = sqliteishBaseName(name);
  if (!base) return null;
  const split = base.indexOf(".g");
  const key = split === -1 ? base : base.slice(0, split);
  return looksLikePartitionKey(key) ? key : null;
}

function inspectPartitionKeyFromName(name: string): string | null {
  if (name.includes(".tmp.")) return null;
  const base = sqliteishBaseName(name);
  return base && looksLikePartitionKey(base) ? base : null;
}

function sqliteishBaseName(name: string): string | null {
  for (const suffix of SQLITE_SUFFIXES) {
    if (name.endsWith(suffix)) {
      return name.slice(0, -suffix.length);
    }
  }
  return null;
}

function looksLikePartitionKey(value: string): boolean {
  return /^[0-9a-fA-F]{16}$/.test(value);
}

function safeReadDir(path: string): string[] {
  try {
    return readdirSync(path).sort((left, right) => left.localeCompare(right));
  } catch {
    return [];
  }
}

function isDirectory(path: string): boolean {
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
}

function safeFileSize(path: string): number {
  try {
    return statSync(path).isFile() ? statSync(path).size : 0;
  } catch {
    return 0;
  }
}
