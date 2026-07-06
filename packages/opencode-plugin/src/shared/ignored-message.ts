import { resolvePromptContext } from "./last-assistant-model.js";

/**
 * Append an `ignored: true` synthetic user message to a session.
 *
 * Used for user-facing informational panels that must NOT trigger an agent
 * turn (e.g. status output, the external-directory restriction notice). The
 * message renders under the current agent (resolved from recent messages) so
 * it shows in the right place in the OpenCode UI, and carries `noReply: true`
 * so no LLM call is made.
 *
 * Model/variant ARE passed when resolvable, mirroring the session's newest
 * context: OpenCode's `createUserMessage` resolves an omitted model as
 * `agent.model ?? session-current ?? default` and PERSISTS the result via
 * `setAgentModel` — so omitting the model on a session whose user picked a
 * non-default model silently resets the session's model/variant (observed on
 * OpenCode Desktop via /aft-status). Passing the resolved current context
 * makes that persistence a no-op. Older OpenCode builds crashed when model
 * was supplied on a `noReply: true` prompt, so on any failure we retry once
 * without model/variant rather than dropping the message.
 */
export async function sendIgnoredMessage(
  client: unknown,
  sessionID: string,
  text: string,
): Promise<void> {
  const typedClient = client as {
    session?: {
      prompt?: (input: unknown) => unknown;
      promptAsync?: (input: unknown) => unknown;
    };
  };

  let agent: string | undefined;
  let model: { providerID: string; modelID: string } | undefined;
  let variant: string | undefined;
  try {
    const ctx = await resolvePromptContext(
      client as Parameters<typeof resolvePromptContext>[0],
      sessionID,
    );
    agent = ctx?.agent;
    model = ctx?.model;
    variant = ctx?.variant;
  } catch {
    agent = undefined;
  }

  const send = async (body: Record<string, unknown>): Promise<void> => {
    const promptInput = { path: { id: sessionID }, body };
    if (typeof typedClient.session?.prompt === "function") {
      await Promise.resolve(typedClient.session.prompt(promptInput));
      return;
    }
    if (typeof typedClient.session?.promptAsync === "function") {
      await typedClient.session.promptAsync(promptInput);
      return;
    }
    throw new Error("[aft-plugin] client.session.prompt is unavailable");
  };

  const base: Record<string, unknown> = {
    noReply: true,
    parts: [{ type: "text", text, ignored: true }],
  };
  if (agent) base.agent = agent;

  if (model) {
    const withModel: Record<string, unknown> = { ...base, model };
    if (variant) withModel.variant = variant;
    try {
      await send(withModel);
      return;
    } catch {
      // Retry below without model/variant (legacy-host compatibility).
    }
  }

  await send(base);
}