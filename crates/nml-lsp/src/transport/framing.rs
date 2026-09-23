use std::io::{BufRead, Write};

use tower_lsp::jsonrpc::Request;

/// One well-formed frame off the wire.
pub(super) enum Frame {
    /// Its body is a JSON-RPC message.
    Message(Box<Request>),
    /// Its body is NOT one. The framing is intact — the whole body was
    /// read — so the stream continues; only the message is lost, and
    /// the answer is a parse error. (Terminating here instead handed a
    /// buggy client one bad body and the server's life.) The NATIVE
    /// transport gives the same answer and then stops reading —
    /// `tokio_util`'s `FramedRead` is fused after a decoder error, so
    /// tower-lsp's loop ends and the process exits 0 — which is
    /// MEASURED and pinned (`tests/index_walk_stdio.rs`,
    /// `one_malformed_frame_ends_the_native_session_with_a_clean_exit_code`),
    /// not a property this pump inherits.
    Unparsable,
}

/// A bogus/corrupt `Content-Length` must never OOM the memory-limited
/// module: cap the body allocation well above any real LSP message (nml
/// config/schema files are KB–low-MB). Over the cap ⇒ treat the stream as
/// corrupt and terminate, rather than allocate gigabytes.
///
/// LIMIT: reach=peer guards=memory surface=editor shown="256 MiB" — bytes of one JSON-RPC frame the wasm transport will allocate
pub(super) const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// One HEADER line's bound. [`MAX_FRAME_BYTES`] caps the body and
/// nothing else: the headers were read with an unbounded
/// `read_line`, so a peer that sent `Content-Length:` and then
/// gigabytes with no newline allocated without limit — the exact
/// OOM of the memory-limited module that bound exists to prevent,
/// reached BEFORE it is consulted. A real header (`Content-Length:
/// 65536`) is tens of bytes. A line that does not end inside its
/// bound leaves no later byte locatable, so the stream is over.
///
/// LIMIT: reach=peer guards=memory surface=editor shown="8 KiB" — bytes of one JSON-RPC header line the wasm transport will read
pub(super) const MAX_HEADER_BYTES: u64 = 8 * 1024;

/// Read one `Content-Length`-framed message. Blocking (the host
/// services the wait). `None` means the STREAM is over: EOF, or a
/// header this reader cannot frame past — after which no later byte
/// can be located, so there is nothing to continue from.
pub(super) fn read_frame(reader: &mut impl BufRead) -> Option<Frame> {
    use std::io::Read as _;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        // Bounded: the header half of the frame, not only the body.
        if reader
            .by_ref()
            .take(MAX_HEADER_BYTES)
            .read_line(&mut line)
            .ok()?
            == 0
        {
            return None; // EOF
        }
        if !line.ends_with('\n') {
            // The line did not end inside its bound, or the stream
            // ended mid-header: no later byte can be located.
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    if content_length == 0 || content_length > MAX_FRAME_BYTES {
        return None;
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(match serde_json::from_slice(&body) {
        Ok(request) => Frame::Message(Box::new(request)),
        Err(_) => Frame::Unparsable,
    })
}

/// Write one framed message body (already serialized).
pub(super) fn write_frame(writer: &mut impl Write, body: &[u8]) {
    let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::{Frame, MAX_FRAME_BYTES, MAX_HEADER_BYTES, read_frame, write_frame};

    /// The frame bound, pinned natively: a well-formed frame
    /// round-trips; a `Content-Length` past [`MAX_FRAME_BYTES`] — or
    /// zero — is a corrupt stream, refused before a byte of body is
    /// allocated (the reader would otherwise reserve the advertised
    /// gigabytes).
    #[test]
    fn a_frame_past_the_bound_is_corrupt_not_an_allocation() {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "method": "initialized", "params": {}
        }))
        .expect("serializes");
        let mut buf = Vec::new();
        write_frame(&mut buf, &body);
        let mut reader = std::io::Cursor::new(buf);
        let Some(Frame::Message(req)) = read_frame(&mut reader) else {
            panic!("a well-formed frame parses");
        };
        assert_eq!(req.method(), "initialized");
        let over = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        assert!(read_frame(&mut std::io::Cursor::new(over.into_bytes())).is_none());
        let zero = b"Content-Length: 0\r\n\r\n".to_vec();
        assert!(read_frame(&mut std::io::Cursor::new(zero)).is_none());
    }

    /// A reader that serves its HEADERS and panics on any read of a
    /// BODY. `is_none()` alone cannot tell the bound's promise from its
    /// absence: a reader with no bytes left reports the same `None`
    /// whether the length was refused up front or the advertised 256 MiB
    /// was allocated and then failed to fill. The read itself is the
    /// assertion.
    struct HeadersOnly(std::io::Cursor<Vec<u8>>);

    impl std::io::Read for HeadersOnly {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!(
                "the reader allocated a body past the frame bound and tried \
                 to fill it — the bound exists to refuse the length, not to \
                 survive a short stream"
            );
        }
    }

    impl std::io::BufRead for HeadersOnly {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.0.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.0.consume(amount);
        }
    }

    /// The bound's actual promise: a `Content-Length` past
    /// [`MAX_FRAME_BYTES`] — and a zero one — is refused while it is
    /// still a NUMBER, before a byte of body is reserved.
    #[test]
    fn a_length_past_the_bound_is_refused_before_a_body_is_allocated() {
        for header in [
            format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1),
            format!("Content-Length: {}\r\n\r\n", usize::MAX),
            "Content-Length: 0\r\n\r\n".to_string(),
        ] {
            let mut reader = HeadersOnly(std::io::Cursor::new(header.clone().into_bytes()));
            assert!(
                read_frame(&mut reader).is_none(),
                "a corrupt length is not a frame: {header:?}"
            );
        }
    }

    /// The HEADER bound, pinned: a header line that does not end
    /// inside [`MAX_HEADER_BYTES`] ends the stream instead of
    /// growing a `String` without limit — pre-fix this read was a
    /// bare `read_line`, so `Content-Length:` followed by bytes
    /// with no newline allocated all of them, under the frame
    /// bound's nose. A header exactly at the bound (its newline the
    /// last byte) still frames.
    #[test]
    fn a_header_line_past_the_bound_ends_the_stream_not_the_heap() {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "method": "initialized", "params": {}
        }))
        .expect("serializes");
        for over in [false, true] {
            let pad = MAX_HEADER_BYTES as usize + usize::from(over);
            // A header the reader ignores, padded to the bound: at
            // the bound its newline is the last byte read; one byte
            // past, the newline is not reached.
            let head = "X-Pad: ";
            let mut wire = format!("{head}{}\r\n", "p".repeat(pad - head.len() - 2)).into_bytes();
            assert_eq!(wire.len(), pad);
            wire.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            wire.extend_from_slice(&body);
            let framed = read_frame(&mut std::io::Cursor::new(wire));
            assert_eq!(framed.is_some(), !over, "pad {pad}");
        }
        // The shape that OOM'd: a header that never ends.
        let endless = vec![b'x'; MAX_HEADER_BYTES as usize * 2];
        assert!(read_frame(&mut std::io::Cursor::new(endless)).is_none());
    }

    /// A well-formed frame whose BODY is not a JSON-RPC message costs
    /// that message, not the stream: the frame is consumed whole and
    /// the NEXT one still reads.
    #[test]
    fn an_unparsable_body_leaves_the_stream_framed() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{ not json");
        write_frame(
            &mut buf,
            &serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "method": "exit"
            }))
            .expect("serializes"),
        );
        let mut reader = std::io::Cursor::new(buf);
        assert!(matches!(read_frame(&mut reader), Some(Frame::Unparsable)));
        let Some(Frame::Message(req)) = read_frame(&mut reader) else {
            panic!("the frame after an unparsable body still reads");
        };
        assert_eq!(req.method(), "exit");
        assert!(read_frame(&mut reader).is_none(), "then EOF");
    }
}
