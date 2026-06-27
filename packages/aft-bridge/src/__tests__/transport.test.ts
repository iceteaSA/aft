/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";

import { BinaryBridge, type BridgeRequestOptions } from "../bridge.js";

class MockBridge extends BinaryBridge {
  readonly calls: Array<{
    command: string;
    params: Record<string, unknown>;
    options: BridgeRequestOptions | undefined;
  }> = [];

  constructor(private readonly response: Record<string, unknown>) {
    super("/tmp/aft-does-not-need-to-exist", process.cwd());
  }

  override async send(
    command: string,
    params: Record<string, unknown> = {},
    options?: BridgeRequestOptions,
  ): Promise<Record<string, unknown>> {
    this.calls.push({ command, params, options });
    return this.response;
  }
}

describe("BinaryBridge toolCall transport", () => {
  test("sends the tool_call envelope and returns the raw response", async () => {
    const rawResponse = {
      id: "42",
      success: true,
      text: "agent output",
      status_bar: { errors: 0, warnings: 1 },
      bg_completions: [{ task_id: "bg-1", status: "completed", exit_code: 0, command: "echo ok" }],
      preview_diff: "diff --git a/file b/file",
    };
    const bridge = new MockBridge(rawResponse);
    const options = { transportTimeoutMs: 1234 };

    const result = await bridge.toolCall(
      "session-123",
      "read",
      { filePath: "README.md", offset: 10 },
      options,
    );

    expect(bridge.calls).toEqual([
      {
        command: "tool_call",
        params: {
          name: "read",
          arguments: { filePath: "README.md", offset: 10 },
          session_id: "session-123",
        },
        options,
      },
    ]);
    expect(result).toBe(rawResponse);
    expect(result.text).toBe("agent output");
    expect(result.status_bar).toEqual({ errors: 0, warnings: 1 });
    expect(result.bg_completions).toEqual(rawResponse.bg_completions);
    expect(result.preview_diff).toBe(rawResponse.preview_diff);
  });
});
