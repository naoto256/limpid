//! Output modules: write processed events to external destinations.

pub(crate) mod batched;
pub mod file;
pub mod http;
pub(crate) mod http_util;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod ltp;
pub mod otlp;
pub(crate) mod persistent_conn;
pub mod stdout;
pub(crate) mod syslog_peers;
pub mod syslog_tcp;
pub mod syslog_udp;
pub mod unix_socket;

/// Concatenate `payload` and a trailing `\n` into a single buffer.
///
/// The line-oriented sinks (`file`, `stdout`, `unix_socket`) all
/// hand this buffer to one `write_all` call rather than issuing
/// `write_all(payload)` and `write_all(b"\n")` back to back. The
/// split shape could return `Ok(())` on the payload and `Err(_)` on
/// the delimiter — leaving an unterminated line that a subsequent
/// retry would follow with a full frame, silently doubling the
/// record. Fusing to one buffer removes the between-writes error
/// boundary; it does NOT make the underlying `write_all` atomic
/// (that keeps looping over `write(2)` internally).
#[inline]
pub(crate) fn frame_with_newline(payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(payload.len() + 1);
    buf.extend_from_slice(payload);
    buf.push(b'\n');
    buf
}

#[cfg(test)]
mod frame_tests {
    use super::frame_with_newline;

    #[test]
    fn frames_empty_payload_as_bare_newline() {
        assert_eq!(frame_with_newline(b""), b"\n");
    }

    #[test]
    fn frames_ascii_payload_with_trailing_newline() {
        assert_eq!(frame_with_newline(b"hello"), b"hello\n");
    }

    #[test]
    fn frames_non_utf8_payload_verbatim() {
        // Non-UTF-8 bytes must not be substituted. This pins the
        // byte-preserving contract every line-oriented sink relies
        // on (silent U+FFFD substitution would break replay
        // fidelity on binary payloads).
        let raw: &[u8] = &[0xff, 0xfe, 0x80, 0x00, 0x0a];
        let framed = frame_with_newline(raw);
        assert_eq!(&framed[..raw.len()], raw);
        assert_eq!(framed.last().copied(), Some(b'\n'));
    }

    #[test]
    fn framed_len_is_payload_plus_one() {
        // Structural pin: the frame is exactly `payload.len() + 1`
        // bytes and the last byte is `\n`. A future refactor that
        // reintroduced a split `write_all(payload) + write_all(b"\n")`
        // would still pass a payload-only buffer through this
        // helper (or bypass it), tripping either this test or the
        // per-sink shape reviews.
        for n in [0usize, 1, 4096, 65_536] {
            let payload = vec![b'x'; n];
            let framed = frame_with_newline(&payload);
            assert_eq!(framed.len(), n + 1);
            assert_eq!(framed.last().copied(), Some(b'\n'));
        }
    }
}
