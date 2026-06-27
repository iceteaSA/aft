import type { BridgeRequestOptions } from "./bridge.js";

export type ToolCallArguments = Record<string, unknown>;

export interface ToolCallResult extends Record<string, unknown> {
  /** Server-rendered agent-facing output added by the `tool_call` command. */
  text: string;
  /** Direct bridge response success flag, carried through unchanged. */
  success: boolean;
  code?: string;
  message?: string;
  status_bar?: unknown;
  bg_completions?: unknown;
}

export interface AftTransportOptions extends BridgeRequestOptions {
  /** Per-call command timeout passed through to BinaryBridge.send. */
  timeoutMs?: number;
  /** Host client used for asynchronous configure-warning delivery. */
  configureWarningClient?: unknown;
  /** Configure command lifecycle override used by BinaryBridge.send. */
  markConfiguredOnSuccess?: boolean;
}

export interface AftTransport<ToolCallContext = string | undefined> {
  /** Lifecycle and raw-command path; tool dispatch uses toolCall instead. */
  send(
    command: string,
    params?: Record<string, unknown>,
    opts?: AftTransportOptions,
  ): Promise<Record<string, unknown>>;

  /**
   * Dispatch a hoisted agent tool through the shared server-side `tool_call`
   * command and return the full raw response, including sidecars.
   */
  toolCall(
    context: ToolCallContext,
    name: string,
    rawArgs: ToolCallArguments,
    opts?: AftTransportOptions,
  ): Promise<ToolCallResult>;
}
