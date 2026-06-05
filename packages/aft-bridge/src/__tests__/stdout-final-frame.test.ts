import { describe, expect, test } from "bun:test";
import { BinaryBridge } from "../bridge.js";

describe("BinaryBridge stdout framing", () => {
  test("parses final push frame without trailing newline when stdout flushes", () => {
    const completions: unknown[] = [];
    const bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      process.cwd(),
      {
        onBashCompletion: (completion) => {
          completions.push(completion);
        },
      },
      { harness: "test" },
    );

    (bridge as any).onStdoutData(
      JSON.stringify({
        type: "bash_completed",
        task_id: "task-final",
        session_id: "s1",
        status: "completed",
        exit_code: 0,
        command: "echo done",
      }),
    );
    (bridge as any).flushStdoutBuffer();

    expect(completions).toHaveLength(1);
    expect((completions[0] as { task_id?: string }).task_id).toBe("task-final");
  });
});
