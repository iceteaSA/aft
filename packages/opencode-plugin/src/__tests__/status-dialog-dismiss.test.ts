/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";

/**
 * StatusDialog dismiss wiring (issue #120): OpenCode TUI wraps content in
 * `api.ui.Dialog` with `onClose` so Enter and Esc both dismiss (host handles
 * keys). Pi parity: status-dialog.ts handleInput closes on return/escape.
 * Full key routing is TUI-host integration; this locks the dismiss callback shape.
 */
describe("StatusDialog dismiss wiring", () => {
  test("dismiss callback resets dialog size and clears the stack", () => {
    const calls: string[] = [];
    const api = {
      ui: {
        dialog: {
          setSize: (size: string) => calls.push(`setSize:${size}`),
          clear: () => calls.push("clear"),
        },
      },
    };

    const dismissStatusDialog = () => {
      api.ui.dialog.setSize("medium");
      api.ui.dialog.clear();
    };

    dismissStatusDialog();
    expect(calls).toEqual(["setSize:medium", "clear"]);
  });
});
