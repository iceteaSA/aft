/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test__ } from "../index.js";
import { buildInspectSections, registerInspectTool } from "../tools/inspect.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

function toolArgs(call: { params: Record<string, unknown> }): Record<string, unknown> {
  return call.params.arguments as Record<string, unknown>;
}

describe("Pi aft_inspect surface", () => {
  test("registers at recommended surface unless explicitly disabled", () => {
    expect(__test__.resolveToolSurface({ tool_surface: "recommended" }).inspect).toBe(true);
    expect(__test__.resolveToolSurface({ tool_surface: "minimal" }).inspect).toBe(false);
    expect(
      __test__.resolveToolSurface({
        tool_surface: "recommended",
        disabled_tools: ["aft_inspect"],
      }).inspect,
    ).toBe(false);
    expect(
      __test__.resolveToolSurface({
        tool_surface: "recommended",
        inspect: { enabled: false },
      }).inspect,
    ).toBe(false);
  });
});

describe("Pi aft_inspect adapter", () => {
  test("declares constrained topK schema and direct-fresh wording", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    const inspect = tools.get("aft_inspect")!;
    const description = inspect.description ?? "";
    expect(description).toContain("diagnostics");
    expect(description).not.toContain("triggered on session idle");
    expect(description).not.toContain("prewarm");
    expect(description).toContain("waits for a fresh reuse scan");
    expect(description).toContain("complete: false");
    expect(description).toContain("deduped background warmup");

    const parameters = inspect.parameters as {
      properties?: Record<string, Record<string, unknown>>;
    };
    expect(parameters.properties?.topK).toMatchObject({
      type: "integer",
      minimum: 1,
      maximum: 100,
      default: 20,
    });
  });

  test("renders diagnostics counts, sentinels, and details defensively", () => {
    const counted = buildInspectSections(
      {
        summary: {
          diagnostics: { errors: 2, warnings: 1, info: 0, hints: 0 },
          metrics: { files: 3, symbols: 4 },
        },
        details: {
          diagnostics: [{ file: "src/app.ts", line: 4, severity: "error", message: "bad type" }],
        },
      },
      { fg: (_name: string, text: string) => text } as never,
    ).join("\n");
    expect(counted).toContain("diagnostics 2 errors/1 warnings/0 info/0 hints");
    expect(counted).toContain("src/app.ts:4 error bad type");

    const environmental = buildInspectSections(
      {
        summary: {
          diagnostics: { errors: 0, warnings: 0, info: 0, hints: 0 },
        },
        details: {
          diagnostics: [
            {
              file: "package.json",
              line: 1,
              severity: "error",
              message: "Failed to load schema from https://example.com/schema.json [environmental]",
            },
          ],
        },
      },
      { fg: (_name: string, text: string) => text } as never,
    ).join("\n");
    expect(environmental).toContain("diagnostics 0 errors");
    expect(environmental).toContain(
      "package.json:1 error Failed to load schema from https://example.com/schema.json [environmental]",
    );

    const pending = buildInspectSections(
      {
        summary: {
          diagnostics: { status: "pending", servers_pending: ["tsserver"] },
        },
      },
      { fg: (_name: string, text: string) => text } as never,
    ).join("\n");
    expect(pending).toContain("diagnostics pending");
    expect(pending).toContain("tsserver");
    expect(pending).not.toContain("0 errors");
  });

  test("sends corrected inspect field names to the bridge", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(
      tools.get("aft_inspect")!,
      { sections: "todos", scope: ["src", "tests"], topK: 9 },
      makeExtContext(projectRoot, "pi-session"),
    );

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toMatchObject({ name: "inspect", session_id: "pi-session" });
    expect(toolArgs(calls[0])).toEqual({
      sections: "todos",
      scope: ["src", "tests"],
      topK: 9,
    });
    expect(calls[0].options).toMatchObject({
      keepBridgeOnTimeout: true,
      timeoutMs: 60_000,
    });
  });

  test("normalizes empty sections and scope sentinels", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(
      tools.get("aft_inspect")!,
      { sections: [], scope: "" },
      makeExtContext(projectRoot, "pi-session"),
    );

    expect(toolArgs(calls[0]).sections).toBeUndefined();
    expect(toolArgs(calls[0]).scope).toBeUndefined();
    expect(toolArgs(calls[0]).topK).toBeUndefined();
  });

  test("fires a quiet Tier 2 run when inspect returns cold pending categories", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "inspect_tier2_run") {
        return new Promise<Record<string, unknown>>(() => {});
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: [],
          pending_categories: ["dead_code", "unused_exports", "duplicates"],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    const result = (await executeTool(
      tools.get("aft_inspect")!,
      {},
      makeExtContext(projectRoot, "pi-session"),
    )) as { details: { scanner_state: { pending_categories: string[] } } };

    expect(result.details.scanner_state.pending_categories).toEqual([
      "dead_code",
      "unused_exports",
      "duplicates",
    ]);
    expect(calls).toHaveLength(2);
    expect(calls[0].command).toBe("tool_call");
    expect(calls[1].command).toBe("inspect_tier2_run");
    expect(calls[1].params).toEqual({
      categories: ["dead_code", "unused_exports", "duplicates"],
      session_id: "pi-session",
    });
  });

  test("fires a quiet Tier 2 run when inspect returns stale categories", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "inspect_tier2_run") {
        return new Promise<Record<string, unknown>>(() => {});
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: ["unused_exports"],
          pending_categories: [],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    const result = (await executeTool(
      tools.get("aft_inspect")!,
      {},
      makeExtContext(projectRoot, "pi-session"),
    )) as { details: { scanner_state: { stale_categories: string[] } } };

    expect(result.details.scanner_state.stale_categories).toEqual(["unused_exports"]);
    expect(calls).toHaveLength(2);
    expect(calls[0].command).toBe("tool_call");
    expect(calls[1].command).toBe("inspect_tier2_run");
    expect(calls[1].params).toEqual({
      categories: ["unused_exports"],
      session_id: "pi-session",
    });
  });

  test("returns the Rust tool_call text body without plugin JSON fallback", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      text: "Duplicates: 2 (top by cost):\n  1083  a.ts == b.ts\nDead code: 1 (rust 1):\n  x.rs::foo",
      summary: { diagnostics: { errors: 1, warnings: 0, info: 0, hints: 2 } },
    }));
    registerInspectTool(api, makePluginContext(bridge));

    const result = (await executeTool(
      tools.get("aft_inspect")!,
      {},
      makeExtContext(projectRoot, "pi-session"),
    )) as { content: Array<{ type: string; text: string }> };
    const text = result.content[0]?.text ?? "";

    // The result text is the raw output; this plugin does not add a diagnostics
    // line or convert the result to a JSON object.
    expect(text).toContain("Duplicates: 2 (top by cost):");
    expect(text).toContain("  x.rs::foo");
    expect(text).not.toContain("diagnostics 1 errors/0 warnings/0 info/2 hints");
    expect(text).not.toContain('"success"');
    expect(text).not.toContain("scanner_state");
  });

  test("second inspect call can read cached Tier 2 data after the trigger starts", async () => {
    const { api, tools } = makeMockApi();
    let tier2RunStarted = false;
    const { bridge, calls } = makeMockBridge((command, params) => {
      if (command === "inspect_tier2_run") {
        tier2RunStarted = true;
        return { success: true, queued_categories: params.categories ?? [] };
      }
      if (tier2RunStarted) {
        return {
          success: true,
          summary: {
            dead_code: { count: 1 },
            unused_exports: { count: 2 },
            duplicates: { count: 3 },
          },
          scanner_state: {
            stale_categories: [],
            pending_categories: [],
          },
        };
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: [],
          pending_categories: ["dead_code", "unused_exports", "duplicates"],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));
    const result = (await executeTool(
      tools.get("aft_inspect")!,
      {},
      makeExtContext(projectRoot, "pi-session"),
    )) as {
      details: {
        summary: { dead_code: { count: number }; unused_exports: { count: number } };
        scanner_state: { pending_categories: string[] };
      };
    };

    expect(result.details.scanner_state.pending_categories).toEqual([]);
    expect(result.details.summary.dead_code.count).toBe(1);
    expect(result.details.summary.unused_exports.count).toBe(2);
    expect(calls.map((call) => call.command)).toEqual([
      "tool_call",
      "inspect_tier2_run",
      "tool_call",
    ]);
  });

  test("does not double-trigger while a Tier 2 run is already in flight", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "inspect_tier2_run") {
        return new Promise<Record<string, unknown>>(() => {});
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: [],
          pending_categories: ["dead_code", "unused_exports", "duplicates"],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));
    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));

    expect(calls.map((call) => call.command)).toEqual([
      "tool_call",
      "inspect_tier2_run",
      "tool_call",
    ]);
    expect(calls[1].params.categories).toEqual(["dead_code", "unused_exports", "duplicates"]);
  });

  test("does not double-trigger a stale Tier 2 category already in flight", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "inspect_tier2_run") {
        return new Promise<Record<string, unknown>>(() => {});
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: ["dead_code"],
          pending_categories: [],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));
    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));

    expect(calls.map((call) => call.command)).toEqual([
      "tool_call",
      "inspect_tier2_run",
      "tool_call",
    ]);
    expect(calls[1].params.categories).toEqual(["dead_code"]);
  });

  test("does not immediately re-trigger a Tier 2 category after a completed run", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) => {
      if (command === "inspect_tier2_run") {
        return { success: true };
      }
      return {
        success: true,
        summary: {},
        scanner_state: {
          stale_categories: ["dead_code"],
          pending_categories: [],
        },
      };
    });
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));
    await Promise.resolve();
    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));

    expect(calls.map((call) => call.command)).toEqual([
      "tool_call",
      "inspect_tier2_run",
      "tool_call",
    ]);
    expect(calls[1].params.categories).toEqual(["dead_code"]);
  });

  test("re-triggers a still-stale Tier 2 category after the cooldown window", async () => {
    const realNow = Date.now;
    let now = 1_000_000;
    Date.now = () => now;
    try {
      const { api, tools } = makeMockApi();
      const { bridge, calls } = makeMockBridge((command) => {
        if (command === "inspect_tier2_run") {
          return { success: true };
        }
        return {
          success: true,
          summary: {},
          scanner_state: {
            stale_categories: ["dead_code"],
            pending_categories: [],
          },
        };
      });
      registerInspectTool(api, makePluginContext(bridge));

      await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));
      await Promise.resolve();
      now += 4 * 60 * 1000 + 1;
      await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));

      expect(calls.map((call) => call.command)).toEqual([
        "tool_call",
        "inspect_tier2_run",
        "tool_call",
        "inspect_tier2_run",
      ]);
      expect(calls[1].params.categories).toEqual(["dead_code"]);
      expect(calls[3].params.categories).toEqual(["dead_code"]);
    } finally {
      Date.now = realNow;
    }
  });

  test("rejects invalid topK without coercion", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    await expect(
      executeTool(
        tools.get("aft_inspect")!,
        { sections: "todos", topK: "9" },
        makeExtContext(projectRoot, "pi-session"),
      ),
    ).rejects.toThrow("topK must be an integer between 1 and 100");
    await expect(
      executeTool(
        tools.get("aft_inspect")!,
        { sections: "todos", topK: 101 },
        makeExtContext(projectRoot, "pi-session"),
      ),
    ).rejects.toThrow("topK must be between 1 and 100");
    expect(calls).toHaveLength(0);
  });
});
