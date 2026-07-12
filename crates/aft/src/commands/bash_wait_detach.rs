use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let detached = ctx.bash_background().signal_wait_mode_detach(req.session());
    // The signal is fire-and-forget from the plugin's user-message hook, so a
    // silent session mismatch is invisible without this trace: log which
    // session the signal resolved against and whether a wait was active.
    crate::slog_info!(
        "bash_wait_detach: session={} detached={} active_wait_sessions={}",
        req.session(),
        detached,
        ctx.bash_background().active_wait_session_count()
    );
    Response::success(&req.id, json!({ "detached": detached }))
}
