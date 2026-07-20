//! Bash command rewriter for hoisted bash.
//!
//! When the agent calls `bash("grep -n foo src/")`, the rewriter detects this
//! pattern, dispatches internally to AFT's `grep` command, and returns the
//! result with a footer hint nudging the agent to use the `grep` tool directly.

pub mod dispatch;
pub mod footer;
pub mod parser;
pub mod rules;

use crate::context::AppContext;
use crate::protocol::Response;
use crate::sandbox_spawn::{native_sandbox_enforced, AuthenticatedPrincipal};

/// A `RewriteRule` matches a specific bash invocation pattern and dispatches
/// internally to an AFT tool.
pub trait RewriteRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, command: &str) -> bool;
    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String>;
}

/// Try to rewrite a bash command into an internal AFT tool call.
/// Returns `Some(response)` if rewritten and `None` when no rule matched or the
/// command must execute inside the native sandbox.
pub fn try_rewrite(
    command: &str,
    session_id: Option<&str>,
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> Option<Response> {
    if native_sandbox_enforced(ctx, principal) {
        return None;
    }
    dispatch::dispatch(command, session_id, ctx)
}
