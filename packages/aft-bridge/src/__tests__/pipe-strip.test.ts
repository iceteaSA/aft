import { describe, expect, test } from "bun:test";
import { maybeStripCompressorPipe } from "../pipe-strip.js";

describe("maybeStripCompressorPipe", () => {
  test("strips bun test piped through grep", () => {
    const result = maybeStripCompressorPipe("bun test | grep fail", true);
    expect(result).toEqual({
      command: "bun test",
      stripped: true,
      note: "[AFT removed `| grep fail`; the output compressor already keeps failures + summary. Pass compressed:false to keep your pipe.]",
    });
  });

  test("strips multi-filter cargo test pipeline", () => {
    const result = maybeStripCompressorPipe("cargo test | grep -A3 FAILED | head", true);
    expect(result.command).toBe("cargo test");
    expect(result.stripped).toBe(true);
    expect(result.note).toContain("| grep -A3 FAILED | head");
  });

  test("does not strip when compression is disabled", () => {
    expect(maybeStripCompressorPipe("bun test | grep fail", false)).toEqual({
      command: "bun test | grep fail",
      stripped: false,
    });
  });

  test("does not strip count grep", () => {
    expect(maybeStripCompressorPipe("bun test | grep -c fail", true)).toEqual({
      command: "bun test | grep -c fail",
      stripped: false,
    });
  });

  test("does not strip when first stage is not a runner", () => {
    expect(maybeStripCompressorPipe("ls | grep foo", true)).toEqual({
      command: "ls | grep foo",
      stripped: false,
    });
  });

  test("does not strip non-noise filters", () => {
    expect(maybeStripCompressorPipe("bun test | sed 's/x/y/'", true)).toEqual({
      command: "bun test | sed 's/x/y/'",
      stripped: false,
    });
  });

  test("does not split on pipes inside quotes", () => {
    expect(maybeStripCompressorPipe('bun test --name "a|b"', true)).toEqual({
      command: 'bun test --name "a|b"',
      stripped: false,
    });
  });

  test("strips known runner forms and rejects cd prefix", () => {
    expect(maybeStripCompressorPipe("npm run test:unit | tail -20", true).command).toBe(
      "npm run test:unit",
    );
    expect(maybeStripCompressorPipe("npx eslint src | head", true).command).toBe("npx eslint src");
    expect(maybeStripCompressorPipe("cd packages/a && bun test | grep fail", true).stripped).toBe(
      false,
    );
  });

  test("does not strip wc or intent-changing grep flags", () => {
    expect(maybeStripCompressorPipe("bun test | wc -l", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("bun test | rg --quiet fail", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("bun test | grep -n fail", true).stripped).toBe(true);
  });

  test("does not treat || as a pipe", () => {
    expect(maybeStripCompressorPipe("bun test || true | grep fail", true).stripped).toBe(false);
  });
});
