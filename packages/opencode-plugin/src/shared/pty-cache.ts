import * as fs from "node:fs/promises";
import { Terminal } from "@xterm/headless";

const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;
const READ_CHUNK_SIZE = 64 * 1024;

export interface PtyTerminalState {
  terminal: Terminal;
  fileHandle: fs.FileHandle;
  offset: number;
  lastAccessMs: number;
}

const terminals = new Map<string, PtyTerminalState>();

export async function getOrCreatePtyTerminal(
  key: string,
  outputPath: string,
): Promise<PtyTerminalState> {
  const existing = terminals.get(key);
  if (existing) {
    existing.lastAccessMs = Date.now();
    return existing;
  }

  const fileHandle = await fs.open(outputPath, "r");
  const state: PtyTerminalState = {
    terminal: new Terminal({ cols: DEFAULT_COLS, rows: DEFAULT_ROWS, allowProposedApi: true }),
    fileHandle,
    offset: 0,
    lastAccessMs: Date.now(),
  };
  terminals.set(key, state);
  return state;
}

export async function readPtyBytes(state: PtyTerminalState): Promise<Buffer> {
  const chunks: Buffer[] = [];
  while (true) {
    const buffer = Buffer.allocUnsafe(READ_CHUNK_SIZE);
    const { bytesRead } = await state.fileHandle.read(buffer, 0, buffer.length, state.offset);
    if (bytesRead === 0) break;
    const chunk = buffer.subarray(0, bytesRead);
    chunks.push(Buffer.from(chunk));
    await writeTerminal(state.terminal, chunk);
    state.offset += bytesRead;
  }
  state.lastAccessMs = Date.now();
  return Buffer.concat(chunks);
}

export async function disposePtyTerminal(key: string): Promise<void> {
  const state = terminals.get(key);
  if (!state) return;
  terminals.delete(key);
  state.terminal.dispose();
  await state.fileHandle.close().catch(() => undefined);
}

export async function disposeAllPtyTerminals(): Promise<void> {
  await Promise.all([...terminals.keys()].map((key) => disposePtyTerminal(key)));
}

export function renderScreen(
  state: PtyTerminalState,
  rows = DEFAULT_ROWS,
  cols = DEFAULT_COLS,
): string {
  const active = state.terminal.buffer.active;
  const lines: string[] = [];
  for (let y = 0; y < rows; y++) {
    const line = active.getLine(active.baseY + y);
    if (!line) {
      lines.push("");
      continue;
    }
    let text = "";
    for (let x = 0; x < cols; x++) {
      text += line.getCell(x)?.getChars() || " ";
    }
    lines.push(text.trimEnd());
  }
  while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
  return lines.join("\n");
}

export function __resetPtyCacheForTests(): void {
  for (const state of terminals.values()) {
    state.terminal.dispose();
    void state.fileHandle.close().catch(() => undefined);
  }
  terminals.clear();
}

export function __ptyCacheSizeForTests(): number {
  return terminals.size;
}

function writeTerminal(terminal: Terminal, data: Uint8Array): Promise<void> {
  return new Promise((resolve) => terminal.write(data, resolve));
}
