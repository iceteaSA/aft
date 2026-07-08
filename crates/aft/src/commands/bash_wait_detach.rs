use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let detached = ctx.bash_background().signal_wait_mode_detach(req.session());
    Response::success(&req.id, json!({ "detached": detached }))
}
