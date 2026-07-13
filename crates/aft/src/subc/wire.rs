//! Frame encoding and writer-queue helpers used by the subc transport edge.

use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Serialize, Serializer};

use super::{
    control_flags, fmt, mpsc, AtomicUsize, DispatchPathMetrics, ErrorBody, Flags, Frame, FrameType,
    Ordering, PathBuf, Response, RouteChannel, ToolCallResult, Value, CONTROL_SEND_TIMEOUT,
    RELIABLE_WRITER_RETRY_INITIAL_BACKOFF, RELIABLE_WRITER_RETRY_MAX_BACKOFF,
};

pub(super) enum WriterEnqueueError {
    Full(Frame),
    Closed,
}

pub(super) fn decrement_counted_channel(counter: &AtomicUsize) {
    let previous = counter.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(previous > 0, "counted channel depth underflow");
}

pub(super) async fn send_counted_channel<T>(
    tx: &mpsc::Sender<T>,
    counter: &AtomicUsize,
    item: T,
) -> Result<(), mpsc::error::SendError<T>> {
    counter.fetch_add(1, Ordering::Relaxed);
    match tx.send(item).await {
        Ok(()) => Ok(()),
        Err(error) => {
            decrement_counted_channel(counter);
            Err(error)
        }
    }
}

pub(super) fn try_enqueue_writer_frame(
    tx: &mpsc::Sender<Frame>,
    metrics: &DispatchPathMetrics,
    frame: Frame,
) -> Result<(), WriterEnqueueError> {
    match tx.try_reserve() {
        Ok(permit) => {
            metrics.writer_queued.fetch_add(1, Ordering::Relaxed);
            permit.send(frame);
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(())) => {
            metrics
                .writer_saturation_count
                .fetch_add(1, Ordering::Relaxed);
            Err(WriterEnqueueError::Full(frame))
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            drop(frame);
            Err(WriterEnqueueError::Closed)
        }
    }
}

pub(super) async fn send_reliable_writer_frame(
    tx: &mpsc::Sender<Frame>,
    metrics: &DispatchPathMetrics,
    mut frame: Frame,
    context: &'static str,
) -> Result<(), SubcError> {
    let mut warned = false;
    let mut backoff = RELIABLE_WRITER_RETRY_INITIAL_BACKOFF;

    loop {
        match try_enqueue_writer_frame(tx, metrics, frame) {
            Ok(()) => return Ok(()),
            Err(WriterEnqueueError::Closed) => return Err(SubcError::WriterClosed),
            Err(WriterEnqueueError::Full(returned_frame)) => {
                frame = returned_frame;
            }
        }

        match tokio::time::timeout(CONTROL_SEND_TIMEOUT, tx.reserve()).await {
            Ok(Ok(permit)) => {
                metrics.writer_queued.fetch_add(1, Ordering::Relaxed);
                permit.send(frame);
                return Ok(());
            }
            Ok(Err(_)) => return Err(SubcError::WriterClosed),
            Err(_) => {
                metrics
                    .writer_saturation_count
                    .fetch_add(1, Ordering::Relaxed);
                if !warned {
                    log::warn!(
                        "subc attach: writer queue stayed full while sending {context}; retrying reliable frame"
                    );
                    warned = true;
                }
                tokio::time::sleep(backoff).await;
                backoff =
                    std::cmp::min(backoff.saturating_mul(2), RELIABLE_WRITER_RETRY_MAX_BACKOFF);
            }
        }
    }
}

pub(super) async fn send_frame(
    tx: &mpsc::Sender<Frame>,
    metrics: &DispatchPathMetrics,
    frame: Frame,
) -> Result<(), SubcError> {
    match try_enqueue_writer_frame(tx, metrics, frame) {
        Ok(()) => Ok(()),
        Err(WriterEnqueueError::Closed) => Err(SubcError::WriterClosed),
        Err(WriterEnqueueError::Full(frame)) => {
            match tokio::time::timeout(CONTROL_SEND_TIMEOUT, tx.reserve()).await {
                Ok(Ok(permit)) => {
                    metrics.writer_queued.fetch_add(1, Ordering::Relaxed);
                    permit.send(frame);
                    Ok(())
                }
                Ok(Err(_)) => Err(SubcError::WriterClosed),
                Err(_) => {
                    metrics
                        .writer_saturation_count
                        .fetch_add(1, Ordering::Relaxed);
                    Err(SubcError::WriterBackpressureTimeout)
                }
            }
        }
    }
}

/// Borrowed flat response matching the standalone NDJSON shape without cloning
/// the response id or any structured data values.
struct FlatToolResponse<'a> {
    response: &'a crate::protocol::Response,
    text: &'a str,
}

impl Serialize for FlatToolResponse<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let data = self.response.data.as_object();
        let has_text = data.is_some_and(|data| data.contains_key("text"));
        let field_count =
            2 + data.map_or(0, |data| {
                data.len()
                    - usize::from(data.contains_key("id"))
                    - usize::from(data.contains_key("success"))
            }) + usize::from(!has_text);
        let mut map = serializer.serialize_map(Some(field_count))?;
        match data.and_then(|data| data.get("id")) {
            Some(value) => map.serialize_entry("id", value)?,
            None => map.serialize_entry("id", &self.response.id)?,
        }
        match data.and_then(|data| data.get("success")) {
            Some(value) => map.serialize_entry("success", value)?,
            None => map.serialize_entry("success", &self.response.success)?,
        }
        if let Some(data) = data {
            for (key, value) in data {
                match key.as_str() {
                    "id" | "success" => {}
                    "text" => map.serialize_entry(key, self.text)?,
                    _ => map.serialize_entry(key, value)?,
                }
            }
        }
        if !has_text {
            map.serialize_entry("text", self.text)?;
        }
        map.end()
    }
}

struct ToolResponseEnvelope<'a> {
    result: &'a ToolCallResult,
}

impl Serialize for ToolResponseEnvelope<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut envelope = serializer.serialize_struct("ToolResponseEnvelope", 3)?;
        envelope.serialize_field(
            "content",
            &[TextContent {
                kind: "text",
                text: &self.result.text,
            }],
        )?;
        envelope.serialize_field("isError", &!self.result.response.success)?;
        envelope.serialize_field(
            "structuredContent",
            &FlatToolResponse {
                response: &self.result.response,
                text: &self.result.text,
            },
        )?;
        envelope.end()
    }
}

#[derive(Serialize)]
struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

pub(super) fn build_tool_response_frame(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    result: &ToolCallResult,
) -> Result<Frame, SubcError> {
    // `content`/`isError` is the MCP-native surface a GENERIC host reads (and a
    // generic host ignores `structuredContent`, per the MCP spec). The
    // FIRST-PARTY AFT plugin instead reads `structuredContent`, which carries
    // the full flat standalone shape ({id, success, ...data, text}) so every
    // structured sidecar the plugin drives UI from — status_bar, bg_completions
    // (in-band drain), preview_diff, code, message, attachments — survives the
    // route. subc relays the body byte-for-byte, so this reaches the plugin
    // unchanged. SubcTransport.toolCall re-lifts `structuredContent` straight to
    // the flat ToolCallResult, so nothing downstream of the transport differs
    // from the NDJSON path.
    let body = serde_json::to_vec(&ToolResponseEnvelope { result }).map_err(SubcError::Json)?;

    Frame::build_with_version(
        ver,
        FrameType::Response,
        flags,
        route.channel,
        route.epoch,
        corr,
        body,
    )
    .map_err(SubcError::FrameBuild)
}

pub(super) fn build_error_frame(
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
) -> Result<Frame, SubcError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(SubcError::Json)?;
    Frame::build_with_version(ver, FrameType::Error, flags, channel, epoch, corr, body)
        .map_err(SubcError::FrameBuild)
}

pub(super) fn build_goodbye_frame(
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
) -> Result<Frame, SubcError> {
    Frame::build_with_version(
        ver,
        FrameType::Goodbye,
        control_flags(),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .map_err(SubcError::FrameBuild)
}

pub(super) fn response_message(response: &Response, fallback: &str) -> String {
    response
        .data
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn response_is_fatal_panic(response: &Response) -> bool {
    !response.success && response.data.get("code").and_then(Value::as_str) == Some("actor_fatal")
}

#[derive(Debug)]
pub enum SubcError {
    Runtime(std::io::Error),
    ConnectionFile {
        path: PathBuf,
        source: subc_transport::ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    InvalidEndpoint {
        path: PathBuf,
        endpoint: String,
    },
    Connect {
        endpoint: String,
        source: std::io::Error,
    },
    Auth {
        endpoint: String,
        source: subc_transport::AuthError,
    },
    FrameIo(subc_transport::FrameIoError),
    FrameBuild(subc_protocol::FrameBuildError),
    WriterClosed,
    WriterBackpressureTimeout,
    WriterJoin(tokio::task::JoinError),
    Json(serde_json::Error),
    ClosedBeforeHelloAck,
    HelloRejected {
        body: Option<ErrorBody>,
    },
    UnexpectedFrame {
        ty: FrameType,
    },
}

impl fmt::Display for SubcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "failed to build subc tokio runtime: {e}"),
            Self::ConnectionFile { path, source } => {
                write!(f, "failed to read subc connection file {path:?}: {source}")
            }
            Self::NoEndpoint { path } => {
                write!(f, "subc connection file {path:?} has no endpoints")
            }
            Self::InvalidEndpoint { path, endpoint } => {
                write!(
                    f,
                    "subc connection file {path:?} has invalid endpoint {endpoint}"
                )
            }
            Self::Connect { endpoint, source } => {
                write!(f, "failed to connect to subc endpoint {endpoint}: {source}")
            }
            Self::Auth { endpoint, source } => {
                write!(
                    f,
                    "failed to authenticate to subc endpoint {endpoint}: {source}"
                )
            }
            Self::FrameIo(e) => write!(f, "subc frame I/O error: {e}"),
            Self::FrameBuild(e) => write!(f, "subc frame build error: {e}"),
            Self::WriterClosed => write!(f, "subc writer task closed"),
            Self::WriterBackpressureTimeout => write!(
                f,
                "subc writer task stayed backpressured while sending a control frame"
            ),
            Self::WriterJoin(e) => write!(f, "subc writer task join error: {e}"),
            Self::Json(e) => write!(f, "subc JSON error: {e}"),
            Self::ClosedBeforeHelloAck => {
                write!(f, "subc daemon closed the connection before HelloAck")
            }
            Self::HelloRejected { body } => match body {
                Some(b) => write!(f, "subc rejected ModuleHello: {} ({})", b.code, b.message),
                None => write!(f, "subc rejected ModuleHello (unparseable error body)"),
            },
            Self::UnexpectedFrame { ty } => {
                write!(f, "subc sent unexpected frame in place of HelloAck: {ty:?}")
            }
        }
    }
}

impl std::error::Error for SubcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc::route_key;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use subc_protocol::PROTOCOL_VERSION;

    #[test]
    fn writer_depth_counter_tracks_enqueued_frames_until_drain() {
        let metrics = DispatchPathMetrics::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(8);

        for corr in 1..=3 {
            let frame = Frame::build(FrameType::Ping, control_flags(), 0, 0, corr, Vec::new())
                .expect("test frame");
            assert!(try_enqueue_writer_frame(&writer_tx, &metrics, frame).is_ok());
        }
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 3);

        for _ in 0..3 {
            writer_rx.try_recv().expect("queued writer frame");
            decrement_counted_channel(&metrics.writer_queued);
        }
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn reliable_writer_send_retries_after_timeout_and_preserves_frame() {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");

        let metrics_for_task = Arc::clone(&metrics);
        let tx_for_task = writer_tx.clone();
        let send_task = tokio::spawn(async move {
            send_reliable_writer_frame(
                &tx_for_task,
                &metrics_for_task,
                Frame::build(FrameType::Pong, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
                "test reliable frame",
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics.writer_saturation_count.load(Ordering::Relaxed) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reliable send should observe a timed-out full writer queue");

        let prefilled = writer_rx.recv().await.expect("prefilled frame");
        assert_eq!(prefilled.header.corr, 1);
        let result = tokio::time::timeout(Duration::from_secs(2), send_task)
            .await
            .expect("reliable send should finish after writer drains")
            .expect("reliable send task should not panic");
        assert!(result.is_ok());
        let delivered = writer_rx.recv().await.expect("retried reliable frame");
        assert_eq!(delivered.header.corr, 2);
    }

    #[test]
    fn response_is_fatal_panic_only_matches_panic_exclusive_code() {
        let tool_error = Response::error("request-1", "internal_error", "ordinary tool error");
        let panic_error = Response::error("request-2", "actor_fatal", "mutating panic");

        assert!(!response_is_fatal_panic(&tool_error));
        assert!(response_is_fatal_panic(&panic_error));
    }

    #[tokio::test]
    async fn control_send_times_out_when_writer_queue_remains_full() {
        let (writer_tx, _writer_rx) = mpsc::channel::<Frame>(1);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");
        let started = Instant::now();

        let result = send_frame(
            &writer_tx,
            &metrics,
            Frame::build(FrameType::Pong, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
        )
        .await;

        assert!(matches!(result, Err(SubcError::WriterBackpressureTimeout)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "control send guard should be bounded"
        );
    }

    #[test]
    fn tool_response_frame_carries_flat_standalone_shape_in_structured_content() {
        use crate::protocol::Response;

        // A response with sidecars the FIRST-PARTY plugin drives UI from
        // (status_bar, bg_completions, code) plus a normal result field.
        let response = Response::success(
            "req-7",
            json!({
                "complete": true,
                "matches": 3,
                "status_bar": { "errors": 0, "warnings": 1 },
                "bg_completions": [{ "task_id": "bash-abc" }],
            }),
        );
        let result = ToolCallResult {
            text: "rendered text".to_string(),
            response,
        };

        // The flat shape must equal the standalone NDJSON `tool_call` body:
        // {id, success, ...data, text}. Build the standalone expectation the
        // same way commands::tool_call::response_with_text does.
        let expected_flat = json!({
            "id": "req-7",
            "success": true,
            "complete": true,
            "matches": 3,
            "status_bar": { "errors": 0, "warnings": 1 },
            "bg_completions": [{ "task_id": "bash-abc" }],
            "text": "rendered text",
        });
        assert_eq!(
            serde_json::to_value(FlatToolResponse {
                response: &result.response,
                text: &result.text,
            })
            .unwrap(),
            expected_flat,
            "structuredContent must be byte-identical to the standalone flat response"
        );

        // The frame body carries the MCP surface for generic hosts AND the flat
        // sidecar shape under structuredContent for the first-party plugin.
        let frame = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            42,
            control_flags(),
            &result,
        )
        .unwrap();
        let expected_body = serde_json::to_vec(&json!({
            "content": [{ "type": "text", "text": "rendered text" }],
            "isError": false,
            "structuredContent": expected_flat.clone(),
        }))
        .unwrap();
        assert_eq!(
            frame.body, expected_body,
            "tool response wire bytes drifted"
        );
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], json!(false));
        assert_eq!(body["content"][0]["type"], json!("text"));
        assert_eq!(body["content"][0]["text"], json!("rendered text"));
        assert_eq!(body["structuredContent"], expected_flat);

        // A failed response flips isError and still carries the flat shape
        // (with success:false + code) for the plugin's error path.
        let err = Response::error_with_data(
            "req-8",
            "ambiguous_match",
            "too many matches",
            json!({ "candidates": ["a", "b"] }),
        );
        let err_result = ToolCallResult {
            text: "error text".to_string(),
            response: err,
        };
        let err_frame = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            43,
            control_flags(),
            &err_result,
        )
        .unwrap();
        let err_body: Value = serde_json::from_slice(&err_frame.body).unwrap();
        assert_eq!(err_body["isError"], json!(true));
        assert_eq!(err_body["structuredContent"]["success"], json!(false));
        assert_eq!(
            err_body["structuredContent"]["code"],
            json!("ambiguous_match")
        );
        assert_eq!(
            err_body["structuredContent"]["candidates"],
            json!(["a", "b"])
        );
        assert_eq!(err_body["structuredContent"]["text"], json!("error text"));
    }
}
