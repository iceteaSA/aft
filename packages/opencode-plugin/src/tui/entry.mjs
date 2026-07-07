// Prefer the host OpenTUI runtime registry when it exists. OpenTUI 0.4.x
// registers these virtual modules process-wide, which lets the precompiled TUI
// use the host's single Solid/OpenTUI runtime even when this package is loaded
// from an npm cache under node_modules.
let mod;
try {
  await import("opentui:runtime-module:" + encodeURIComponent("@opentui/solid"));
  mod = await import("../tui-compiled/index.tsx");
} catch {
  // Older hosts and bare Bun do not provide the virtual registry. Falling back
  // to the raw TSX entry keeps development checkouts and OpenTUI 0.3.x hosts
  // working where the Solid transform still applies to this source path.
  mod = await import("./index.tsx");
}

export default mod.default;
