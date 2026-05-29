<h1 align="center">AFT — Agent File Toolkit</h1>

<p align="center">
  <a href="https://crates.io/crates/agent-file-tools"><img src="https://img.shields.io/crates/v/agent-file-tools?label=crate&color=blue&style=flat-square" alt="crates.io"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft"><img src="https://img.shields.io/npm/v/@cortexkit/aft?label=cli&color=blue&style=flat-square" alt="npm @cortexkit/aft"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft-opencode"><img src="https://img.shields.io/npm/v/@cortexkit/aft-opencode?label=opencode&color=blue&style=flat-square" alt="npm @cortexkit/aft-opencode"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft-pi"><img src="https://img.shields.io/npm/v/@cortexkit/aft-pi?label=pi&color=blue&style=flat-square" alt="npm @cortexkit/aft-pi"></a>
  <a href="https://discord.gg/DSa65w8wuf"><img src="https://img.shields.io/discord/1488852091056295957?style=flat-square&logo=discord&logoColor=white&label=Discord&color=5865F2" alt="Discord"></a>
  <a href="https://github.com/cortexkit/aft/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-green?style=flat-square" alt="MIT License"></a>
</p>

<p align="center">
  <a href="#what-is-aft">What is AFT?</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#key-capabilities">Key Capabilities</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#development">Development</a> ·
  <a href="#tool-reference">Tool Reference</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="https://discord.gg/DSa65w8wuf">💬 Discord</a>
</p>

---

## What is AFT?

AI coding agents are fast at reasoning but clumsy at interacting with code. The typical pattern — read an entire file to find one function, construct a diff from memory, apply it by line number, hope nothing shifted — burns tokens on context noise and breaks when the file changes.

AFT is the tooling layer that fixes this. Part of the CortexKit family, it acts as the **motor cortex and sensory cortex** for coding agents — giving them precise, low-level control over code files and a proper operating system to run in.

Concretely:

- **Sensory cortex** — structural code understanding. Agents can outline a file in one call, zoom into a specific function by name, and search code by meaning instead of guessing keywords.
- **Motor cortex** — precise code manipulation. Edit functions by name instead of line number. Refactor across the whole workspace with one command. The binary handles parsing, formatting, backup, and type-checking.
- **Agent OS** — background bash tasks, PTY sessions, output compression. Agents can spawn long-running work, inspect it later, and get compressed output instead of raw firehose.

The result is less token waste. Instead of reading 500 lines to find one function, the agent calls `aft_outline` then `aft_zoom` — ~40 tokens instead of ~375. Instead of guessing where a concept lives, `aft_search` finds the exact location by meaning. Bash output is compressed per-command. Refactoring that would need manual grep+read+edit across 10 files becomes one `aft_refactor move` call.

AFT ships as a Rust binary with thin adapters for [OpenCode](https://opencode.ai) and [Pi](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent). It hoists itself into the host harness's built-in tool slots — agents keep calling the same tool names (`read`, `write`, `edit`, `bash`, `grep`), but now backed by tree-sitter parsing, indexed search, output compression, and symbol-aware operations.

---

## Quick Start

```bash
npx @cortexkit/aft setup
```

This auto-detects which harnesses you have installed and configures each one. On the next session start, the `aft` binary downloads if needed and all tools become available.

Add `--harness opencode` or `--harness pi` to target a specific harness.

**What AFT does to each harness:**
- **OpenCode** — replaces built-in `read`, `write`, `edit`, `apply_patch`, `ast_grep_search`, `ast_grep_replace`, and `lsp_diagnostics` with AFT-powered versions and adds the `aft_` family on top.
- **Pi** — replaces built-in `read`, `write`, `edit`, and `grep` and adds the `aft_` family on top.

See the [CLI reference](docs/cli.md) for `doctor`, `doctor --fix`, `doctor lsp`, and cache management commands.

---

## Key Capabilities

### Structural code understanding

- **`aft_outline`** — list every symbol in a file (or directory, or remote URL) with kind, name, line range, and visibility. One call instead of reading the whole file.
- **`aft_zoom`** — inspect a specific function, class, or type with call-graph annotations (what it calls, what calls it). ~40 tokens instead of ~375.
- **`aft_search`** — find code by meaning when grep keywords fall short. Hybrid semantic + lexical retrieval over an indexed codebase. Requires ONNX Runtime for the local embedding backend.
- **`aft_callgraph`** — follow callers, callees, data flow, impact analysis, and shortest call paths between two symbols across the workspace.
- **`aft_inspect`** — codebase-health snapshot: TODOs, file/symbol metrics, dead code, unused exports, and duplicate detection in one call.
- **`grep` / `glob`** — trigram-indexed regex search and file discovery. Background index building with disk persistence.

### Precise manipulation

- **`edit`** — find/replace with fuzzy matching, or replace a named symbol directly. Batch edits, multi-file transactions, glob replace across matching files.
- **`write`** — write a file with auto-directory creation, backup, formatting, and inline LSP diagnostics.
- **`apply_patch`** — multi-file `*** Begin Patch` format with atomic rollback.
- **`aft_refactor`** — workspace-wide symbol move (updates all imports), function extraction, function inlining.
- **`aft_import`** — language-aware import add, remove, and organize (TS/JS/TSX/Python/Rust/Go).
- **`aft_transform`** — structural transforms: add class members, Rust derives, Python decorators, Go struct tags, try/catch wrapping.
- **`ast_grep_search` / `ast_grep_replace`** — structural code search and replace using AST patterns with meta-variables.
- **`lsp_diagnostics`** — on-demand errors and warnings from language servers. Not a full type checker, but fast feedback during edits.

### Agent OS

- **`bash`** — unified shell execution with command rewriting (cat→read, grep→grep tool, etc.), per-command output compression, background task spawning, and tree-sitter permission scanning (OpenCode).
- **Background bash** — spawn detached tasks with `background: true`, inspect with `bash_status`, kill with `bash_kill`. Output is buffered and compressed. Long foreground commands auto-promote to background.
- **Bash compression** — three-tier output compression: built-in Rust compressors for git/cargo/npm/bun/pnpm/pytest/tsc/biome, declarative TOML filters for the long tail (make/ls/find/du/docker/kubectl/gh/etc.), and generic ANSI-strip + dedup fallback.
- **PTY** — interactive terminal sessions for REPLs and terminal apps (python, node, vim, even a nested agent). Drive with `bash_write`, inspect rendered screen state via `bash_status`.
- **`bash_watch`** — block on or asynchronously watch a background/PTY task for an output pattern or exit.

### Safety and recovery

- **`aft_safety`** — per-file undo stack, named checkpoints, restore to any checkpoint. Every edit is backed up.
- **Auto-backup** — every write and edit saves a snapshot before mutating.
- **Auto-format** — edits run the project formatter (biome, rustfmt, prettier, etc.) automatically.
- **On-demand diagnostics** — pass `diagnostics: true` on a write/edit to get LSP errors inline, or call `aft_inspect` / `lsp_diagnostics` at a verification checkpoint.

---

## Supported Languages

| Language | Outline | Edit | Imports | Refactor |
|----------|---------|------|---------|---------|
| TypeScript / TSX | ✓ | ✓ | ✓ | ✓ |
| JavaScript / JSX | ✓ | ✓ | ✓ | ✓ |
| Python | ✓ | ✓ | ✓ | ✓ |
| Rust | ✓ | ✓ | ✓ | partial |
| Go | ✓ | ✓ | ✓ | partial |
| C / C++ / C# | ✓ | ✓ | — | — |
| Java / Kotlin / Scala | ✓ | ✓ | — | — |
| Swift | ✓ | ✓ | — | — |
| Ruby | ✓ | ✓ | — | — |
| PHP | ✓ | ✓ | — | — |
| Lua / Perl | ✓ | ✓ | — | — |
| Zig | ✓ | ✓ | — | — |
| Bash | ✓ | ✓ | — | — |
| HTML / Markdown / JSON | ✓ | ✓ | — | — |
| Solidity | ✓ | ✓ | — | — |
| Vue | ✓ | ✓ | — | — |

Every listed language works with `aft_outline`, `aft_zoom`, `read`/`edit`/`write`, and the structural tool surface. AST search and replace covers TS/JS/TSX, Python, Rust, Go, C, C++, C#, Zig, Solidity, and Vue. Import management covers TS/JS/TSX, Python, Rust, and Go.

---

## Architecture

AFT is a Rust binary driven by thin adapter packages per harness. The binary speaks a simple JSON-over-stdio request/response protocol — one process per session stays alive for the session lifetime.

```
   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
   │  OpenCode   │    │     Pi      │    │  Future…    │
   │   agent     │    │   agent     │    │  (MCP, …)   │
   └──────┬──────┘    └──────┬──────┘    └──────┬──────┘
           │ tool calls       │ tool calls       │
           ▼                  ▼                  ▼
   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
   │ aft-opencode │   │   aft-pi     │   │     …        │  ← thin adapters per harness
   │  (TS plugin) │   │  (TS plugin) │   │              │    Hoist tools, manage
   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘    BridgePool, resolve binary
           │                  │                  │
           └──────────────────┴──────────────────┘
                              │
                              │ JSON-over-stdio
                              ▼
                   ┌────────────────────────┐
                   │     aft binary         │  ← shared core
                   │       (Rust)           │
                   ├────────────────────────┤
                   │ • tree-sitter (17 lang)│
                   │ • symbols & call graph │
                   │ • diff/format/backup   │
                   │ • LSP client           │
                   │ • trigram index        │
                   │ • semantic index       │
                   └────────────────────────┘
```

Per-harness adapter responsibilities:
- **Hoist** the harness's built-in tool slots and register AFT-only tools.
- **Manage a BridgePool** — one persistent `aft` process per session for warm parse trees and isolated undo history.
- **Resolve the binary** — cache → npm platform package → PATH → cargo install → GitHub release download.
- **Translate** between the harness's plugin API and AFT's request/response protocol.

AFT data lives under a shared CortexKit storage root (`~/.local/share/cortexkit/aft/`). Backups, search indexes, and downloaded LSP servers persist there across sessions.

See the [tool reference](docs/tools.md) for complete documentation of every tool.

---

## Development

AFT is a monorepo: bun workspaces for TypeScript, cargo workspace for Rust.

**Requirements:** Bun ≥ 1.0, Rust stable toolchain (1.80+).

```sh
# Install JS dependencies
bun install

# Build the Rust binary
cargo build --release

# Build the TypeScript plugin
bun run build

# Run all tests
bun run test        # TypeScript tests
cargo test          # Rust tests

# Lint and format
bun run lint        # biome check
bun run lint:fix    # biome check --write
bun run format      # biome format + cargo fmt
```

**Project layout:**

```
opencode-aft/
├── crates/
│   └── aft/              # Rust binary — shared core (tree-sitter, search, LSP, etc.)
│       └── src/
├── packages/
│   ├── aft-cli/          # Unified CLI (@cortexkit/aft) — setup/doctor across all harnesses
│   ├── opencode-plugin/  # OpenCode adapter (@cortexkit/aft-opencode)
│   │   └── src/
│   │       ├── tools/    # One file per tool group
│   │       ├── config.ts # Config loading and schema
│   │       └── downloader.ts
│   ├── pi-plugin/        # Pi adapter (@cortexkit/aft-pi)
│   │   └── src/
│   └── npm/              # Platform-specific binary packages
└── scripts/
    └── version-sync.mjs  # Keeps npm and cargo versions in sync
```

---

## Contributing

Pull requests for bugs are welcome. For features, broader fixes that requires architectural changes, please open an issue first to discuss the approach.

The binary protocol is documented in `crates/aft/src/main.rs`. Adding a new command means implementing it in Rust and adding a corresponding tool definition (or extending an existing one) in each harness adapter (`packages/opencode-plugin/src/tools/` and `packages/pi-plugin/src/tools/`).

Run `bun run format` and `cargo fmt` before submitting. The CI will reject unformatted code.

---

## License

[MIT](LICENSE)

---

## Separate documentation

- [Tool reference](docs/tools.md) — complete documentation for every tool
- [Configuration](docs/config.md) — config schema, LSP, auto-install
- [CLI commands](docs/cli.md) — setup, doctor, and cache management
- [Search benchmarks](docs/benchmarks.md) — trigram index vs ripgrep comparison
