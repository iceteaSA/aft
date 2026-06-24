/** Shared pure formatting helpers for host bash tool wrappers. */

/** Render a millisecond duration as a compact seconds string (8000 -> "8s", 5500 -> "5.5s"). */
export function formatSeconds(ms: number): string {
  return `${Number((ms / 1000).toFixed(1))}s`;
}

export function formatForegroundResult(data: Record<string, unknown>): string {
  const output = (data.output_preview as string | undefined) ?? "";
  const outputPath = data.output_path as string | undefined;
  const truncated = data.output_truncated === true;
  const status = data.status as string | undefined;
  const exit = data.exit_code as number | undefined;
  let rendered = output;
  if (truncated && outputPath) {
    rendered += `\n[output truncated; full output at ${outputPath}]`;
  }
  if (status === "timed_out") {
    rendered += `\n[command timed out]`;
  }
  if (typeof exit === "number" && exit !== 0) {
    rendered += `\n[exit code: ${exit}]`;
  }
  return rendered;
}

export function isTerminalStatus(status: unknown): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed" || status === "timed_out"
  );
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
