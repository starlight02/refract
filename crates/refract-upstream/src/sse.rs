//! SSE 流处理。
//!
//! 两条路径：
//! - [`sse_stream`]：解析成事件，供协议转换用。
//! - [`byte_stream`]：原始字节透传，供同协议直通用。
//!
//! 两者都套了**空闲超时**。这不是可选项：上游静默挂起（TCP 连接还在但不再
//! 发数据）在真实流量里很常见，没有空闲超时的话客户端会永远等下去，连接
//! 也永远不释放。

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use refract_core::{ErrorKind, GatewayError};

/// 一个 SSE 事件。
///
/// 只保留网关需要的三个字段。`id` 与 `retry` 是浏览器 EventSource 的断线重连
/// 机制，LLM API 全都不用，带上它们只是噪音。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// 事件名。OpenAI 系不发这个字段，Anthropic 系每帧都发。
    pub event: String,
    /// 数据载荷。
    pub data: String,
}

impl SseEvent {
    /// 该帧是否是流终止标记 `data: [DONE]`。
    pub fn is_done_sentinel(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// 解析后的 SSE 事件流。
pub type SseStream = Pin<Box<dyn Stream<Item = Result<SseEvent, GatewayError>> + Send>>;

/// 原始字节流。
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, GatewayError>> + Send>>;

/// 给流套上空闲超时。
///
/// 与 `tokio::time::timeout` 包住整个流不同：这里每收到一项就重置计时器，
/// 所以长回答不会被误杀，而真正卡死的连接会在 `idle` 之后报 `Timeout`。
struct IdleTimeout<S> {
    inner: S,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl<S> IdleTimeout<S> {
    fn new(inner: S, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl<S, T, E> Stream for IdleTimeout<S>
where
    S: Stream<Item = Result<T, E>> + Unpin,
{
    type Item = Result<T, IdleError<E>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 先轮询数据：有数据就重置计时器。顺序很重要 —— 反过来会在数据与
        // 超时同时就绪时错误地报超时。
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                let idle = self.idle;
                self.sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + idle);
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(IdleError::Inner(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(IdleError::Idle(self.idle)))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

enum IdleError<E> {
    Inner(E),
    Idle(Duration),
}

/// 把 reqwest 的字节流解析成 SSE 事件流。
pub fn sse_stream<S>(bytes: S, idle: Duration) -> SseStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    use eventsource_stream::Eventsource as _;
    use futures_util::StreamExt as _;

    let events = bytes.eventsource();
    let guarded = IdleTimeout::new(events, idle);

    Box::pin(guarded.map(|item| match item {
        Ok(event) => Ok(SseEvent {
            event: event.event,
            data: event.data,
        }),
        Err(IdleError::Idle(d)) => Err(idle_error(d)),
        Err(IdleError::Inner(e)) => Err(GatewayError::new(
            ErrorKind::UpstreamError,
            format!("upstream stream failed: {e}"),
        )),
    }))
}

/// 原始字节透传，仅套空闲超时。
pub fn byte_stream<S>(bytes: S, idle: Duration) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    use futures_util::StreamExt as _;

    let guarded = IdleTimeout::new(bytes, idle);
    Box::pin(guarded.map(|item| match item {
        Ok(chunk) => Ok(chunk),
        Err(IdleError::Idle(d)) => Err(idle_error(d)),
        Err(IdleError::Inner(e)) => Err(GatewayError::new(
            ErrorKind::UpstreamError,
            format!("upstream stream failed: {e}"),
        )),
    }))
}

fn idle_error(idle: Duration) -> GatewayError {
    GatewayError::new(
        ErrorKind::Timeout,
        format!("upstream stalled: no data for {}s", idle.as_secs().max(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    /// 把一串 chunk 变成 reqwest 风格的流。
    fn chunks(items: Vec<&'static str>) -> impl Stream<Item = reqwest::Result<Bytes>> + Unpin {
        futures_util::stream::iter(
            items
                .into_iter()
                .map(|s| Ok(Bytes::from_static(s.as_bytes()))),
        )
    }

    #[tokio::test]
    async fn parses_openai_style_frames_without_event_names() {
        let stream = sse_stream(
            chunks(vec![
                "data: {\"a\":1}\n\n",
                "data: {\"a\":2}\n\n",
                "data: [DONE]\n\n",
            ]),
            Duration::from_secs(5),
        );
        let events: Vec<_> = stream.map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(
            events[0].event, "message",
            "eventsource defaults to `message`"
        );
        assert!(events[2].is_done_sentinel());
    }

    #[tokio::test]
    async fn parses_anthropic_style_named_frames() {
        let stream = sse_stream(
            chunks(vec![
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
            ]),
            Duration::from_secs(5),
        );
        let events: Vec<_> = stream.map(|e| e.unwrap()).collect().await;
        assert_eq!(events[0].event, "message_start");
        assert_eq!(events[1].event, "content_block_delta");
    }

    #[tokio::test]
    async fn handles_frames_split_across_chunks() {
        // TCP 不保证 chunk 边界与 SSE 帧边界对齐 —— 真实流量里帧被切开很常见。
        let stream = sse_stream(
            chunks(vec!["data: {\"a\"", ":1}\n", "\ndata: {\"b\":2}\n\n"]),
            Duration::from_secs(5),
        );
        let events: Vec<_> = stream.map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "{\"b\":2}");
    }

    #[tokio::test]
    async fn multiline_data_is_joined_with_newlines() {
        let stream = sse_stream(
            chunks(vec!["data: line1\ndata: line2\n\n"]),
            Duration::from_secs(5),
        );
        let events: Vec<_> = stream.map(|e| e.unwrap()).collect().await;
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[tokio::test]
    async fn comment_only_frames_are_skipped() {
        // 上游常发 `: keep-alive` 心跳，它不是事件，不该冒泡给解码器。
        let stream = sse_stream(
            chunks(vec![": keep-alive\n\n", "data: {\"a\":1}\n\n"]),
            Duration::from_secs(5),
        );
        let events: Vec<_> = stream.map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[tokio::test]
    async fn idle_timeout_fires_when_upstream_stalls() {
        // 一个永不产出的流：模拟上游连接还在但不再发数据。
        let stalled = futures_util::stream::pending::<reqwest::Result<Bytes>>();
        let mut stream = byte_stream(stalled, Duration::from_millis(50));

        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.message.contains("stalled"));
    }

    #[tokio::test]
    async fn idle_timer_resets_on_each_item() {
        use futures_util::stream::StreamExt as _;

        // 每 30ms 一个 chunk，共 5 个，空闲上限 100ms。
        // 若计时器不重置，总耗时 150ms 会触发超时。
        let ticker = futures_util::stream::unfold(0_u8, |n| async move {
            if n >= 5 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            Some((Ok(Bytes::from_static(b"x")), n + 1))
        })
        .boxed();

        let stream = byte_stream(ticker, Duration::from_millis(100));
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 5);
        assert!(
            items.iter().all(|i| i.is_ok()),
            "a resetting idle timer must not fire while data keeps arriving"
        );
    }

    #[tokio::test]
    async fn byte_stream_passes_chunks_through_verbatim() {
        let stream = byte_stream(
            chunks(vec!["event: a\ndata: 1\n\n", "data: 2\n\n"]),
            Duration::from_secs(5),
        );
        let joined: Vec<u8> = stream
            .map(|c| c.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        // 直通模式必须字节级一致：重新编码会丢掉我们不认识的新字段。
        assert_eq!(
            String::from_utf8(joined).unwrap(),
            "event: a\ndata: 1\n\ndata: 2\n\n"
        );
    }

    #[tokio::test]
    async fn done_sentinel_detection_tolerates_whitespace() {
        assert!(
            SseEvent {
                event: "message".into(),
                data: " [DONE] ".into()
            }
            .is_done_sentinel()
        );
        assert!(
            !SseEvent {
                event: "message".into(),
                data: "[DONE_NOT]".into()
            }
            .is_done_sentinel()
        );
    }
}
