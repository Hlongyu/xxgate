use bytes::Bytes;
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures::{FutureExt, Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use xxgate_core::{Error, Result};

pub const FORMAT: &str = "openai.responses.v1";

pub struct SseDecoder {
    tx: mpsc::UnboundedSender<Result<Bytes>>,
    events: Pin<Box<dyn Stream<Item = std::result::Result<Event, EventStreamError<Error>>> + Send>>,
    buffer: Vec<u8>,
    line_bytes: usize,
    skip_lf: bool,
    prefix: Vec<u8>,
    started: bool,
}

impl Default for SseDecoder {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes>>();
        Self {
            tx,
            events: Box::pin(UnboundedReceiverStream::new(rx).eventsource()),
            buffer: vec![],
            line_bytes: 0,
            skip_lf: false,
            prefix: vec![],
            started: false,
        }
    }
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8], limit: usize) -> Result<Vec<String>> {
        let mut events = vec![];
        for &byte in bytes {
            if !self.started {
                // eventsource-stream 0.2.3 slices an initial UTF-8 BOM at a non-character
                // boundary. Strip only the stream's leading BOM before handing it data.
                self.prefix.push(byte);
                if [0xef, 0xbb, 0xbf].starts_with(&self.prefix) {
                    if self.prefix.len() == 3 {
                        self.prefix.clear();
                        self.started = true;
                    }
                    continue;
                }
                self.started = true;
                for prefix in std::mem::take(&mut self.prefix) {
                    self.byte(prefix, limit, &mut events)?;
                }
            } else {
                self.byte(byte, limit, &mut events)?;
            }
        }
        Ok(events)
    }
    fn byte(&mut self, byte: u8, limit: usize, events: &mut Vec<String>) -> Result<()> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(());
            }
        }
        self.skip_lf = byte == b'\r';
        self.buffer.push(if byte == b'\r' { b'\n' } else { byte });
        if self.buffer.len() > limit {
            return Err(Error::new(
                502,
                "sse_event_too_large",
                "Upstream SSE event exceeds the configured limit",
            ));
        }
        if byte != b'\r' && byte != b'\n' {
            self.line_bytes += 1;
            return Ok(());
        }
        if std::mem::take(&mut self.line_bytes) > 0 {
            return Ok(());
        }
        self.tx
            .send(Ok(Bytes::from(std::mem::take(&mut self.buffer))))
            .map_err(|_| Error::new(502, "sse_closed", "Upstream SSE parser closed"))?;
        while let Some(Some(event)) = self.events.next().now_or_never() {
            let event = event
                .map_err(|_| Error::new(502, "invalid_sse", "Upstream returned invalid SSE"))?;
            if event.data != "[DONE]" {
                events.push(event.data);
            }
        }
        Ok(())
    }
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len() + self.prefix.len()
    }
}

pub fn frame(value: &serde_json::Value) -> Result<Bytes> {
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::new(502, "invalid_sse_event", "SSE event has no type"))?;
    if kind.len() > 128
        || !kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Error::new(
            502,
            "invalid_sse_event",
            "SSE event type is invalid",
        ));
    }
    let data = serde_json::to_string(value)
        .map_err(|_| Error::new(502, "invalid_sse_event", "Cannot serialize SSE event"))?;
    Ok(Bytes::from(format!("event: {kind}\ndata: {data}\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handles_fragmented_utf8_crlf_and_comment_heartbeats() {
        let mut s = SseDecoder::default();
        let data="\u{feff}: ping\r\n\r\nevent: test\r\ndata: {\"type\":\"test\",\"text\":\"你好\"}\r\n\r\n".as_bytes();
        let mut events = vec![];
        for byte in data {
            events.extend(s.push(&[*byte], 4096).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("你好"));
    }
    #[test]
    fn event_limit_does_not_accumulate_framing_or_comment_bytes() {
        let mut decoder = SseDecoder::default();
        for _ in 0..1000 {
            assert!(decoder.push(b": ping\n\n", 100).unwrap().is_empty());
            assert_eq!(decoder.push(b"data: {}\n\n", 100).unwrap(), vec!["{}"]);
            assert_eq!(decoder.buffered_bytes(), 0);
        }
        assert!(decoder.push(&[b'a'; 101], 100).is_err());
    }
}
