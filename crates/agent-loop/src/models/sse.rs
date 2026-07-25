//! Incremental SSE decoder: feed raw response bytes, get back the payload of
//! every complete `data:` line seen (`[DONE]` included — callers decide).
//! Buffers partial lines across chunks; `event:` lines and comments are
//! skipped (both OpenAI and Anthropic put the event type inside the JSON).
//!
//! Ported verbatim from `llm-client`.

#[derive(Default)]
pub(crate) struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self { Self::default() }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(payload) = parse_sse_line(&line) {
                out.push(payload);
            }
        }
        out
    }

    /// Flush a trailing line not terminated by `\n` at end-of-stream.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        parse_sse_line(&rest).into_iter().collect()
    }
}

/// A complete SSE line is valid UTF-8 (a multibyte sequence never contains a
/// `\n` byte), but decode lossily anyway — a corrupt line is skipped, not fatal.
fn parse_sse_line(line: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches('\r').trim();
    let data = line.strip_prefix("data:")?.trim_start();
    if data.is_empty() { None } else { Some(data.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn sse_decoder_buffers_partial_lines_across_chunks() {
        let mut dec = SseDecoder::new();
        assert!(dec.feed(br#"data: {"a": 1"#).is_empty());
        assert_eq!(dec.feed(b"}\r\n").len(), 1);
    }

    #[test]
    fn sse_decoder_skips_events_comments_and_keeps_done() {
        let mut dec = SseDecoder::new();
        let out = dec.feed(b"event: message_start\n: ping\n\ndata: {\"type\":\"ping\"}\ndata: [DONE]\n");
        assert_eq!(out, vec!["{\"type\":\"ping\"}".to_string(), "[DONE]".to_string()]);
        assert!(dec.finish().is_empty());
    }

    #[test]
    fn sse_decoder_finish_flushes_unterminated_tail() {
        let mut dec = SseDecoder::new();
        assert!(dec.feed(b"data: tail-without-newline").is_empty());
        assert_eq!(dec.finish(), vec!["tail-without-newline".to_string()]);
    }

    #[test]
    fn sse_decoder_handles_multibyte_split() {
        // "€" is 3 bytes in UTF-8; split across the chunk boundary.
        let payload = "data: {\"t\":\"€\"}\n".as_bytes();
        let (a, b) = payload.split_at(12);
        let mut dec = SseDecoder::new();
        let (first, second) = (dec.feed(a), dec.feed(b));
        assert!(first.is_empty());
        assert_eq!(second.len(), 1);
    }
}
