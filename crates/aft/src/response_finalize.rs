use crate::context::AppContext;
use crate::protocol::Response;

/// Apply finalizers in the established response order: background completions first, then status bar counts.
pub fn finalize_response(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
) {
    finalize_response_with_bg_completions(response, ctx, session_id, attach_command, true);
}

pub fn finalize_response_with_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
) {
    if allow_bg_completions {
        attach_bg_completions(response, ctx, session_id, attach_command);
    }
    attach_status_bar(response, ctx, attach_command);
}

pub enum DispatchOutcome {
    Immediate(Response),
    Deferred(PendingResponse),
}

pub type PendingResponsePoll = Box<dyn FnMut(&AppContext) -> Option<Response>>;

pub struct PendingResponse {
    pub request_id: String,
    pub session_id: String,
    pub attach_command: String,
    pub poll: PendingResponsePoll,
}

pub struct ResolvedPending {
    pub response: Response,
    pub session_id: String,
    pub attach_command: String,
}

#[derive(Default)]
pub struct PendingResponses {
    entries: Vec<PendingResponse>,
}

impl PendingResponses {
    pub fn register(&mut self, pending: PendingResponse) {
        self.entries
            .retain(|entry| entry.request_id != pending.request_id);
        self.entries.push(pending);
    }

    pub fn poll_ready(&mut self, ctx: &AppContext) -> Vec<ResolvedPending> {
        let mut ready = Vec::new();
        let mut waiting = Vec::with_capacity(self.entries.len());

        for mut pending in self.entries.drain(..) {
            if let Some(response) = (pending.poll)(ctx) {
                ready.push(ResolvedPending {
                    response,
                    session_id: pending.session_id,
                    attach_command: pending.attach_command,
                });
            } else {
                waiting.push(pending);
            }
        }

        self.entries = waiting;
        ready
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn drain_on_shutdown(&mut self) {
        self.entries.clear();
    }
}

pub fn attach_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if matches!(
        command,
        "configure"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_regex_match"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    if !ctx
        .bash_background()
        .has_completions_for_session(Some(session_id))
    {
        return;
    }
    let completions = ctx
        .bash_background()
        .drain_completions_for_session(Some(session_id));
    if completions.is_empty() {
        return;
    }
    let value = serde_json::json!(completions);
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("bg_completions".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "bg_completions": value });
        }
    }
}

/// Attach the agent status-bar counts to the response envelope so the plugin
/// after-hook can surface the IDE-style status bar (emit-on-change). Skips
/// internal/transport commands that don't represent agent tool calls (their
/// responses never reach the agent, and bash-lifecycle commands fire rapidly).
/// `errors`/`warnings` are read live from the LSP store here; Tier-2/todos are
/// last-known. Omitted entirely until the Tier-2 cache is populated once.
pub fn attach_status_bar(response: &mut Response, ctx: &AppContext, command: &str) {
    if matches!(
        command,
        "configure"
            | "ping"
            | "version"
            | "status"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_regex_match"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    let Some(counts) = ctx.status_bar_counts() else {
        return;
    };
    if !ctx.should_emit_status_bar(&counts) {
        return;
    }
    let value = serde_json::json!({
        "errors": counts.errors,
        "warnings": counts.warnings,
        "dead_code": counts.dead_code,
        "unused_exports": counts.unused_exports,
        "duplicates": counts.duplicates,
        "todos": counts.todos,
        "tier2_stale": counts.tier2_stale,
    });
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("status_bar".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "status_bar": value });
        }
    }
}
