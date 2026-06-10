import { describe, expect, test } from "bun:test";
import { clampSemanticTimeout } from "../bridge.js";
import { LONG_RUNNING_COMMAND_TIMEOUT_MS } from "../command-timeouts.js";

/**
 * Regression: the clamp used to compare against the bridge DEFAULT timeout
 * (30s), but `semantic_search` actually runs with the 60s per-command
 * transport override — so a user's `semantic.timeout_ms: 60000` (LMStudio 8B
 * cold-load headroom) was silently cut to 25s, and background refresh batches
 * (no transport constraint at all) were aborted mid-embed.
 */
describe("clampSemanticTimeout", () => {
  const budget = LONG_RUNNING_COMMAND_TIMEOUT_MS.semantic_search; // 60s
  const margin = 5_000;

  test("clamps against the semantic transport budget, not the 30s bridge default", () => {
    const result = clampSemanticTimeout({ semantic: { timeout_ms: 60_000 } }, 30_000);
    const semantic = result.semantic as { timeout_ms: number };
    // 60s budget - 5s margin = 55s. The old behavior produced 25s.
    expect(semantic.timeout_ms).toBe(budget - margin);
  });

  test("leaves values within the budget untouched", () => {
    const overrides = { semantic: { timeout_ms: 50_000 }, other: "x" };
    expect(clampSemanticTimeout(overrides, 30_000)).toBe(overrides);
  });

  test("uses the bridge timeout when it exceeds the per-command budget", () => {
    const result = clampSemanticTimeout({ semantic: { timeout_ms: 300_000 } }, 120_000);
    const semantic = result.semantic as { timeout_ms: number };
    expect(semantic.timeout_ms).toBe(120_000 - margin);
  });

  test("ignores configs without a numeric semantic.timeout_ms", () => {
    const noSemantic = { foo: 1 };
    expect(clampSemanticTimeout(noSemantic, 30_000)).toBe(noSemantic);
    const nonNumeric = { semantic: { timeout_ms: "60s" } };
    expect(clampSemanticTimeout(nonNumeric, 30_000)).toBe(nonNumeric);
    const arr = { semantic: [1] };
    expect(clampSemanticTimeout(arr, 30_000)).toBe(arr);
  });

  test("preserves sibling semantic fields when clamping", () => {
    const result = clampSemanticTimeout(
      { semantic: { timeout_ms: 600_000, backend: "openai_compatible" } },
      30_000,
    );
    const semantic = result.semantic as Record<string, unknown>;
    expect(semantic.backend).toBe("openai_compatible");
    expect(semantic.timeout_ms).toBe(budget - margin);
  });
});
