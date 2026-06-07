import { describe, expect, test } from "bun:test";
import { resolveBashKillTimeout } from "../bash-timeout.js";

describe("resolveBashKillTimeout", () => {
  const WINDOW = 8_000;

  test("undefined timeout stays undefined (bridge applies its 30-min default)", () => {
    expect(resolveBashKillTimeout(undefined, WINDOW)).toBeUndefined();
  });

  test("timeout below the foreground wait window is treated as unset (#102)", () => {
    // The reporter's case: model passed 100ms with a large window.
    expect(resolveBashKillTimeout(100, 120_000)).toBeUndefined();
    expect(resolveBashKillTimeout(100, WINDOW)).toBeUndefined();
    // Just under the window is still incoherent.
    expect(resolveBashKillTimeout(WINDOW - 1, WINDOW)).toBeUndefined();
  });

  test("timeout at or above the window is honored verbatim", () => {
    expect(resolveBashKillTimeout(WINDOW, WINDOW)).toBe(WINDOW);
    expect(resolveBashKillTimeout(60_000, WINDOW)).toBe(60_000);
    expect(resolveBashKillTimeout(30 * 60 * 1000, WINDOW)).toBe(30 * 60 * 1000);
  });
});
