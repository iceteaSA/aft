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

  test("call_tree collapses unresolved leaf children by default", () => {
    const text = formatCallgraphSections("call_tree", {
      name: "entry",
      file: "/repo/src/a.ts",
      line: 1,
      resolved: true,
      children: [
        { name: "len", file: "/repo/src/a.ts", line: 2, resolved: false, children: [] },
        { name: "Some", file: "/repo/src/a.ts", line: 3, resolved: false, children: [] },
        { name: "len", file: "/repo/src/a.ts", line: 4, resolved: false, children: [] },
        { name: "assert", file: "/repo/src/a.ts", line: 5, resolved: false, children: [] },
        {
          name: "realCallee",
          file: "/repo/src/b.ts",
          line: 10,
          resolved: true,
          children: [],
        },
        {
          name: "wrapping_add",
          file: "/repo/src/a.ts",
          line: 6,
          resolved: false,
          children: [],
        },
        { name: "as_ref", file: "/repo/src/a.ts", line: 7, resolved: false, children: [] },
        { name: "as_ptr", file: "/repo/src/a.ts", line: 8, resolved: false, children: [] },
        { name: "load", file: "/repo/src/a.ts", line: 9, resolved: false, children: [] },
        { name: "lock", file: "/repo/src/a.ts", line: 11, resolved: false, children: [] },
        { name: "Err", file: "/repo/src/a.ts", line: 12, resolved: false, children: [] },
        { name: "cfg", file: "/repo/src/a.ts", line: 13, resolved: false, children: [] },
        { name: "panic", file: "/repo/src/a.ts", line: 14, resolved: false, children: [] },
      ],
    }).join("\n");

    expect(text).toContain(
      "↳ + 12 unresolved external calls: len, Some, assert, wrapping_add, as_ref, as_ptr, load, lock, Err, cfg, … (+1 more)",
    );
    expect(text.match(/unresolved external calls/g) ?? []).toHaveLength(1);
    expect(text).toContain("realCallee [/repo/src/b.ts:10]");
    expect(text).not.toContain("len [/repo/src/a.ts:2] [unresolved]");
    expect(text).not.toContain("panic [/repo/src/a.ts:14] [unresolved]");
  });

  test("call_tree includeUnresolved renders every unresolved callee individually", () => {
    const text = formatCallgraphSections(
      "call_tree",
      {
        name: "entry",
        file: "/repo/src/a.ts",
        line: 1,
        resolved: true,
        children: [
          { name: "realCallee", file: "/repo/src/b.ts", line: 10, resolved: true, children: [] },
          { name: "missing", file: "/repo/src/a.ts", line: 3, resolved: false, children: [] },
          { name: "len", file: "/repo/src/a.ts", line: 4, resolved: false, children: [] },
        ],
      },
      undefined,
      { includeUnresolved: true },
    ).join("\n");

    expect(text).toContain("missing [/repo/src/a.ts:3] [unresolved]");
    expect(text).toContain("len [/repo/src/a.ts:4] [unresolved]");
    expect(text).not.toContain("unresolved external calls");
    // Resolved callee carries no marker.
    expect(text).toContain("realCallee [/repo/src/b.ts:10]");
    expect(text).not.toContain("realCallee [/repo/src/b.ts:10] [unresolved]");
  });

  test("call_tree resolved-only output is unchanged by unresolved collapse option", () => {
    const payload = {
      name: "entry",
      file: "/repo/src/a.ts",
      line: 1,
      resolved: true,
      children: [
        { name: "realCallee", file: "/repo/src/b.ts", line: 10, resolved: true, children: [] },
      ],
    };
    const collapsed = formatCallgraphSections("call_tree", payload).join("\n");
    const expanded = formatCallgraphSections("call_tree", payload, undefined, {
      includeUnresolved: true,
    }).join("\n");

    expect(collapsed).toBe(expanded);
    expect(collapsed).toContain("realCallee [/repo/src/b.ts:10]");
    expect(collapsed).not.toContain("unresolved external calls");
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

  test("callers renders hub-summary hidden-test guidance", () => {
    const text = formatCallgraphSections("callers", {
      total_callers: 49,
      hub_summary: {
        message: "Next: 49 callers (41 in tests, hidden — pass includeTests) — narrow with scope",
      },
      callers: [],
    }).join("\n");
    expect(text).toContain("49 callers");
    expect(text).toContain("41 in tests, hidden — pass includeTests");
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

  test("trace_to renders budgeted path counts as lower bounds", () => {
    const text = formatCallgraphSections("trace_to", {
      total_paths: 7,
      total_paths_is_lower_bound: true,
      entry_points_found: 2,
      hub_summary: {
        message: "Next: at least 7 paths — showing 2; traversal capped; narrow with scope",
      },
      paths: [],
    }).join("\n");
    expect(text).toContain("at least 7 paths");
    expect(text).toContain("Next: at least 7 paths");
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

  test("callers marks name_match edges with ~ and leaves exact unmarked", () => {
    const text = formatCallgraphSections("callers", {
      total_callers: 2,
      callers: [
        {
          file: "/repo/a.ts",
          callers: [
            { symbol: "exactFn", line: 10 },
            { symbol: "maybeFn", line: 20, resolved_by: "name_match" },
          ],
        },
      ],
    }).join("\n");
    expect(text).toContain("↳ exactFn:10");
    expect(text).not.toMatch(/↳ exactFn:10 ~/);
    expect(text).toContain("↳ maybeFn:20 ~");
  });

  test("callers does not mark type_match edges", () => {
    const text = formatCallgraphSections("callers", {
      total_callers: 1,
      callers: [
        {
          file: "/repo/a.ts",
          callers: [{ symbol: "typedFn", line: 5, resolved_by: "type_match" }],
        },
      ],
    }).join("\n");
    expect(text).toContain("↳ typedFn:5");
    expect(text).not.toMatch(/↳ typedFn:5 ~/);
  });

  test("impact and trace_to_symbol mark name_match on edge lines", () => {
    const impactText = formatCallgraphSections("impact", {
      total_affected: 2,
      affected_files: 1,
      callers: [
        { caller_symbol: "exactCaller", caller_file: "/repo/a.ts", line: 1 },
        {
          caller_symbol: "nameCaller",
          caller_file: "/repo/a.ts",
          line: 2,
          resolved_by: "name_match",
        },
      ],
    }).join("\n");
    expect(impactText).toContain("↳ exactCaller");
    expect(impactText).not.toContain("↳ exactCaller ~");
    expect(impactText).toContain("↳ nameCaller ~");

    const traceText = formatCallgraphSections("trace_to_symbol", {
      path: [
        { symbol: "hopExact", file: "/repo/a.ts", line: 1 },
        { symbol: "hopName", file: "/repo/b.ts", line: 2, resolved_by: "name_match" },
      ],
    }).join("\n");
    expect(traceText).toContain("hopExact");
    expect(traceText).not.toMatch(/hopExact.*~/);
    expect(traceText).toMatch(/hopName.*~/);
  });

  test("name_match marker uses theme fg warning", () => {
    const roles: string[] = [];
    const theme: CallgraphTheme = {
      fg: (role, text) => {
        roles.push(`${role}:${text}`);
        return `[${role}]${text}`;
      },
    };
    const text = formatCallgraphSections(
      "callers",
      {
        total_callers: 1,
        callers: [
          {
            file: "/repo/a.ts",
            callers: [{ symbol: "fn", line: 1, resolved_by: "name_match" }],
          },
        ],
      },
      theme,
    ).join("\n");
    expect(text).toContain("[warning]~");
    expect(roles).toContain("warning:~");
  });

  test("call_tree preserves name_match markers on resolved edges when siblings collapse", () => {
    const text = formatCallgraphSections("call_tree", {
      name: "entry",
      file: "/repo/a.ts",
      line: 1,
      resolved: true,
      children: [
        {
          name: "nameOnly",
          file: "/repo/b.ts",
          line: 9,
          resolved: true,
          resolved_by: "name_match",
          children: [],
        },
        {
          name: "missing",
          file: "/repo/a.ts",
          line: 3,
          resolved: false,
          children: [],
        },
      ],
    }).join("\n");
    expect(text).toContain("nameOnly [/repo/b.ts:9] ~");
    expect(text).toContain("+ 1 unresolved external call: missing");
    expect(text).not.toContain("[unresolved] ~");
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
