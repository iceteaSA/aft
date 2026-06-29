import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { BridgePool } from "../pool.js";
import { SubcTransportPool } from "../subc-transport.js";
import { createAftTransportPool } from "../transport-factory.js";

describe("createAftTransportPool", () => {
  let dir: string;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "aft-transport-factory-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  const baseOpts = () => ({
    harness: "opencode",
    binaryPath: "/nonexistent/aft", // never spawned in these tests
    poolOptions: {} as never,
    configOverrides: {},
  });

  test("no subc.connection_file ⇒ standalone BridgePool (the default)", async () => {
    const pool = await createAftTransportPool(baseOpts());
    expect(pool).toBeInstanceOf(BridgePool);
    await pool.shutdown();
  });

  test("empty/whitespace subc.connection_file ⇒ standalone (not subc)", async () => {
    const pool = await createAftTransportPool({ ...baseOpts(), subcConnectionFile: "   " });
    expect(pool).toBeInstanceOf(BridgePool);
    await pool.shutdown();
  });

  test("present + existing connection file ⇒ SubcTransportPool", async () => {
    const connFile = join(dir, "subc-connection.json");
    writeFileSync(connFile, JSON.stringify({ port: 1, token: "x" }));
    const pool = await createAftTransportPool({
      ...baseOpts(),
      subcConnectionFile: connFile,
    });
    expect(pool).toBeInstanceOf(SubcTransportPool);
    await pool.shutdown();
  });

  test("present but MISSING connection file ⇒ FAILS LOUD (no silent downgrade)", async () => {
    const missing = join(dir, "does-not-exist.json");
    await expect(
      createAftTransportPool({ ...baseOpts(), subcConnectionFile: missing }),
    ).rejects.toThrow(/no subc.*connection file exists/i);
  });

  test("the fail-loud error names the path and the remedy", async () => {
    const missing = join(dir, "absent.json");
    let message = "";
    try {
      await createAftTransportPool({ ...baseOpts(), subcConnectionFile: missing });
    } catch (err) {
      message = err instanceof Error ? err.message : String(err);
    }
    expect(message).toContain(missing);
    expect(message).toContain("Start the Subconscious daemon");
    expect(message).toContain("remove subc.connection_file");
  });
});
