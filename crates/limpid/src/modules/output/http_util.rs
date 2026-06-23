//! Shared helpers for HTTP-based outputs (`output http`, `output
//! otlp_http`). Lives here rather than in each module so the
//! invariants stay consistent across them — every place that buffers
//! a peer's response body for an error diagnostic must apply the same
//! byte cap, and adding a third HTTP sink shouldn't require
//! rediscovering this.

/// Hard cap on how many bytes of an error response body we read into
/// memory for the failure diagnostic. A malicious or misconfigured
/// peer can otherwise return an unbounded body and `response.text()`
/// would buffer the whole thing before we trim it. 4 KiB is plenty
/// for the typical "what went wrong" snippet a downstream operator
/// needs to see in the daemon log.
pub(crate) const ERROR_BODY_BYTE_CAP: usize = 4096;

/// Drain at most `cap` bytes from the response, then stop. Uses
/// `Response::chunk()` (available without the `stream` reqwest
/// feature) so we don't have to pull in `futures-util` for one
/// streaming consumer.
///
/// Note: when we break mid-chunk after hitting `cap`, the `Response`
/// is dropped without reaching EOF. reqwest/hyper closes the
/// underlying connection in that case rather than returning it to
/// the keep-alive pool — that's an accepted trade-off here, the
/// error path is rare and bounded memory matters more than reusing
/// a connection to a peer that just failed.
pub(crate) async fn read_body_capped(mut response: reqwest::Response, cap: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cap.min(1024));
    while buf.len() < cap {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = cap - buf.len();
                let take = chunk.len().min(remaining);
                buf.extend_from_slice(&chunk[..take]);
                if chunk.len() > take {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    buf
}
