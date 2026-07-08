/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { execFile } from "node:child_process";
import { writeFile } from "node:fs/promises";
import { sep } from "node:path";
import { promisify } from "node:util";
import {
  cleanupHarnesses,
  cleanupSharedSubcRig,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  type PreparedBinary,
  prepareBinary,
  prepareSubcHarness,
} from "./helpers.js";

process.env.AFT_OPENCODE_E2E_IMPORT_ONLY = "1";
const [
  { runEditWriteToolcallSuite },
  { runReadOnlySpineToolcallSuite },
  { runZoomToolcallSuite },
  { runCallgraphToolcallSuite },
  { runHonestReportingSuite },
  { runApplyPatchRollbackSuite },
  { runFormatOnEditApplyPatchSuite },
  { runSafetySuite },
] = await Promise.all([
  import("./edit-write-toolcall.test.js"),
  import("./read-only-spine-toolcall.test.js"),
  import("./zoom-toolcall.test.js"),
  import("./callgraph-toolcall.test.js"),
  import("./honest-reporting.test.js"),
  import("./apply-patch-rollback.test.js"),
  import("./format-on-edit-apply-patch.test.js"),
  import("./safety.test.js"),
]).finally(() => {
  delete process.env.AFT_OPENCODE_E2E_IMPORT_ONLY;
});

const execFileAsync = promisify(execFile);

const initialBinary = await prepareBinary();
const initialSubc = await prepareSubcHarness(initialBinary);
const skipReason = initialBinary.binaryPath
  ? initialSubc.skipReason
  : (initialBinary.skipReason ?? "aft binary unavailable");
const maybeDescribe = skipReason ? describe.skip : describe;
const describeName = skipReason
  ? `subc transport parity sweep (skipped: ${skipReason})`
  : "subc transport parity sweep";

maybeDescribe(describeName, () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnessFactory: HarnessFactory = (prepared, options) =>
    createHarness(prepared, { ...options, transport: "subc" });

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
    const subc = await prepareSubcHarness(preparedBinary);
    if (subc.skipReason) throw new Error(subc.skipReason);
  }, 30_000);

  afterAll(async () => {
    await cleanupSharedSubcRig();
  });

  runEditWriteToolcallSuite({ harnessFactory, name: "subc parity: edit/write tool_call" });
  runReadOnlySpineToolcallSuite({ harnessFactory, name: "subc parity: read-only spine tool_call" });
  runZoomToolcallSuite({ harnessFactory, name: "subc parity: zoom tool_call" });
  runCallgraphToolcallSuite({ harnessFactory, name: "subc parity: callgraph tool_call" });
  runHonestReportingSuite({ harnessFactory, name: "subc parity: honest reporting" });
  runApplyPatchRollbackSuite({ harnessFactory, name: "subc parity: apply_patch rollback" });
  runFormatOnEditApplyPatchSuite({
    harnessFactory,
    name: "subc parity: format_on_edit apply_patch",
    skipSubcWriteSidecarGaps: true,
  });
  // SUBC GAP: safety/undo still depends on native commands that the subc AFT
  // tool manifest does not admit (`checkpoint`, `restore_checkpoint`,
  // `list_checkpoints`, `edit_history`, `edit_match`, `delete_file`,
  // `undo_preview`, and `undo`). Keep this suite explicit but skipped until the
  // production manifest/gate grows those safety commands.
  runSafetySuite({
    harnessFactory,
    name: "subc parity: safety/undo (SUBC GAP: native safety commands missing)",
    skipSubcNativeCommandGaps: true,
  });

  test("server-rendered text matches NDJSON for representative tool calls", async () => {
    const harnesses: E2EHarness[] = [];
    try {
      const ndjson = await createHarness(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-parity-ndjson-",
      });
      const subc = await harnessFactory(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
        tempPrefix: "aft-plugin-parity-subc-",
      });
      harnesses.push(ndjson, subc);
      await Promise.all([seedParityFixture(ndjson), seedParityFixture(subc)]);

      const calls: Array<{ name: string; args: Record<string, unknown> }> = [
        { name: "read", args: { filePath: "sample.ts" } },
        { name: "grep", args: { pattern: "subc_parity_marker", path: "." } },
        { name: "outline", args: { target: "sample.ts" } },
        { name: "zoom", args: { filePath: "sample.ts", symbols: "parityTarget" } },
        { name: "inspect", args: {} },
        { name: "edit", args: { filePath: "edit.txt", oldString: "before", newString: "after" } },
      ];

      for (const call of calls) {
        // Transient index/store building states are honest output, not parity
        // gaps — poll BOTH sides to the converged state before comparing. A
        // side that never converges still fails the assertion verbatim.
        const converged = (text: string) => !text.includes("building/retrying");
        const ndjsonText = await toolTextUntil(ndjson, call.name, call.args, converged);
        const subcText = await toolTextUntil(subc, call.name, call.args, converged);
        expect(normalizeRoot(subcText, subc.tempDir), call.name).toBe(
          normalizeRoot(ndjsonText, ndjson.tempDir),
        );
      }
    } finally {
      await cleanupHarnesses(harnesses);
    }
  }, 90_000);
});

async function seedParityFixture(harness: E2EHarness): Promise<void> {
  await writeFile(
    harness.path("sample.ts"),
    [
      "export const marker = 'subc_parity_marker';",
      "export function parityTarget(input: string): string {",
      "  return input.trim();",
      "}",
      "// TODO parity inspect marker",
      "",
    ].join("\n"),
    "utf8",
  );
  await writeFile(harness.path("edit.txt"), "before\n", "utf8");
  await gitInitFixture(harness.tempDir);
}

async function gitInitFixture(root: string): Promise<void> {
  const git = (args: string[]) => execFileAsync("git", args, { cwd: root });
  await git(["init"]);
  await git(["config", "user.email", "aft-tests@example.invalid"]);
  await git(["config", "user.name", "AFT Tests"]);
  await git(["add", "."]);
  await git(["commit", "-m", "initial fixture"]);
}

async function toolText(
  harness: E2EHarness,
  name: string,
  args: Record<string, unknown>,
): Promise<string> {
  const response = await harness.bridge.toolCall(`parity-${name}`, name, args);
  expect(response.success, `${name}: ${JSON.stringify(response)}`).toBe(true);
  return response.text;
}

/** toolText, re-polled (1s cadence, 30s budget) until `ready` accepts the
 *  rendered text. Returns the last text either way — a side that never
 *  converges produces a mismatch the assertion reports verbatim. */
async function toolTextUntil(
  harness: E2EHarness,
  name: string,
  args: Record<string, unknown>,
  ready: (text: string) => boolean,
): Promise<string> {
  const deadline = Date.now() + 30_000;
  let text = await toolText(harness, name, args);
  while (!ready(text) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    text = await toolText(harness, name, args);
  }
  return text;
}

function normalizeRoot(text: string, root: string): string {
  const escapedRoot = root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const slashRoot = root.split(sep).join("/");
  const escapedSlashRoot = slashRoot.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return text
    .replace(new RegExp(escapedRoot, "g"), "<ROOT>")
    .replace(new RegExp(escapedSlashRoot, "g"), "<ROOT>");
}
