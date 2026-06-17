import { describe, expect, test } from "bun:test";
import {
  type CallgraphTheme,
  formatCallgraphSections,
  PLAIN_CALLGRAPH_THEME,
} from "../callgraph-format.js";

describe("formatCallgraphSections", () => {
  test("call_tree renders nested children and depth warning", () => {
    const sections = formatCallgraphSections("call_tree", {
      name: "run",
      file: "/repo/src/a.ts",
      line: 1,
      depth_limited: true,
      truncated: 2,
      children: [{ name: "helper", file: "/repo/src/a.ts", line: 4, children: [] }],
    });
    const text = sections.join("\n");
    expect(text).toContain("run");
    expect(text).toContain("helper");
    expect(text).toContain("2 truncated");
  });

  test("call_tree marks unresolved callees and leaves resolved ones unmarked", () => {
    const text = formatCallgraphSections("call_tree", {
      name: "entry",
      file: "/repo/src/a.ts",
      line: 1,
      resolved: true,
      children: [
        { name: "realCallee", file: "/repo/src/b.ts", line: 10, resolved: true, children: [] },
        { name: "missing", file: "/repo/src/a.ts", line: 3, resolved: false, children: [] },
      ],
    }).join("\n");
    // Unresolved callee: file/line is the callsite, not a definition — must be flagged.
    expect(text).toContain("missing [/repo/src/a.ts:3] [unresolved]");
    // Resolved callee carries no marker.
    expect(text).toContain("realCallee [/repo/src/b.ts:10]");
    expect(text).not.toContain("realCallee [/repo/src/b.ts:10] [unresolved]");
  });

  test("callers collapses repeated symbols and keeps true total in summary", () => {
    const sections = formatCallgraphSections("callers", {
      total_callers: 16,
      depth_limited: true,
      truncated: 63,
      callers: [
        {
          file: "/repo/src/handler.ts",
          callers: [
            { symbol: "maybeFireHistorian", line: 3060 },
            { symbol: "<top-level>", line: 202 },
            { symbol: "<top-level>", line: 228 },
            { symbol: "<top-level>", line: 257 },
            { symbol: "otherFn", line: 99 },
          ],
        },
      ],
    });
    const text = sections.join("\n");
    expect(text).toContain("16 callers");
    expect(text).toContain("1 file group");
    expect(text).toContain("63 truncated");
    expect(text).toContain("↳ maybeFireHistorian:3060");
    expect(text).toContain("↳ <top-level>:202, 228, 257");
    expect(text).toContain("↳ otherFn:99");
    expect(text).not.toContain("line ");
  });

  test("trace_to_symbol renders hops", () => {
    const text = formatCallgraphSections("trace_to_symbol", {
      path: [{ symbol: "main", file: "/repo/a.ts", line: 1 }],
    }).join("\n");
    expect(text).toContain("1 hop");
    expect(text).toContain("main");
  });

  test("trace_to renders paths", () => {
    const text = formatCallgraphSections("trace_to", {
      total_paths: 1,
      entry_points_found: 1,
      paths: [{ hops: [{ symbol: "main", file: "/repo/a.ts", line: 1, is_entry_point: true }] }],
    }).join("\n");
    expect(text).toContain("1 path");
    expect(text).toContain("Path 1");
  });

  test("impact lists affected sites", () => {
    const text = formatCallgraphSections("impact", {
      total_affected: 1,
      affected_files: 1,
      callers: [
        {
          caller_symbol: "main",
          caller_file: "/repo/a.ts",
          line: 7,
          call_expression: "run()",
        },
      ],
    }).join("\n");
    expect(text).toContain("1 affected call site");
    expect(text).toContain("↳ main");
    expect(text).toContain("run()");
  });

  test("trace_data renders hops", () => {
    const text = formatCallgraphSections("trace_data", {
      hops: [
        {
          file: "/repo/a.ts",
          symbol: "run",
          variable: "x",
          line: 3,
          flow_type: "parameter",
        },
      ],
    }).join("\n");
    expect(text).toContain("1 hop");
    expect(text).toContain("x");
  });

  test("custom theme fg is invoked", () => {
    const roles: string[] = [];
    const theme: CallgraphTheme = {
      fg: (role, text) => {
        roles.push(role);
        return `[${role}]${text}`;
      },
    };
    formatCallgraphSections("callers", { total_callers: 0, callers: [] }, theme);
    expect(roles.length).toBeGreaterThan(0);
    expect(roles).toContain("success");
  });

  test("plain theme matches PLAIN_CALLGRAPH_THEME default", () => {
    const payload = {
      total_callers: 1,
      callers: [{ file: "/a.ts", callers: [{ symbol: "f", line: 1 }] }],
    };
    const a = formatCallgraphSections("callers", payload).join("\n");
    const b = formatCallgraphSections("callers", payload, PLAIN_CALLGRAPH_THEME).join("\n");
    expect(a).toBe(b);
  });
});
