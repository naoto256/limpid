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

/// Maximum characters shown for a redirect destination, excluding the
/// field label, quotes and optional truncation marker.
const LOCATION_MAX_CHARS: usize = 200;

/// Show a redirect destination without URL credentials, query or fragment.
/// Resolve relative references against the response URL; never echo an
/// unparseable value or a non-HTTP URL, which may itself contain secrets.
pub(crate) fn location_snippet(
    headers: &reqwest::header::HeaderMap,
    base: &reqwest::Url,
) -> String {
    let Some(value) = headers.get(reqwest::header::LOCATION) else {
        return String::new();
    };
    let Some(mut url) = value.to_str().ok().and_then(|value| base.join(value).ok()) else {
        return "; Location: <invalid URL>".into();
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return "; Location: <non-HTTP URL>".into();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    let mut chars = url.as_str().chars();
    let mut snippet: String = chars.by_ref().take(LOCATION_MAX_CHARS).collect();
    if chars.next().is_some() {
        snippet.push('…');
    }
    format!("; Location: {snippet:?}")
}

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

/// Build a human-readable snippet of an error response body for the
/// failure diagnostic, capped at `cap` bytes of input and
/// `max_chars` characters of output. When the peer (or an upstream
/// proxy) advertises a Content-Encoding limpid doesn't decode —
/// limpid's `reqwest` is built without the `gzip` / `brotli` /
/// `deflate` features, so `Response::chunk()` returns the still-
/// encoded raw bytes — surface a placeholder instead of running
/// `from_utf8_lossy` over compressed gibberish that ends up as
/// �replacement-char soup in the daemon log. The raw byte count is
/// kept in the placeholder so an operator can tell "the peer is
/// returning something, just not something we can render".
pub(crate) async fn error_snippet(
    response: reqwest::Response,
    cap: usize,
    max_chars: usize,
) -> String {
    let encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let raw = read_body_capped(response, cap).await;
    match encoding.as_deref() {
        Some(enc) if !enc.is_empty() && enc != "identity" => {
            format!("<{}-encoded body, {} bytes>", enc, raw.len())
        }
        _ => String::from_utf8_lossy(&raw)
            .chars()
            .take(max_chars)
            .collect(),
    }
}

/// Request construction errors are deterministic; connection and timeout
/// failures can recover. Preserve diagnostic context without losing policy.
pub(crate) fn request_error(error: reqwest::Error, context: String) -> super::batched::SendError {
    let permanent = error.is_builder();
    let error = anyhow::Error::new(error).context(context);
    if permanent {
        super::batched::SendError::Permanent(error)
    } else {
        super::batched::SendError::retryable(error)
    }
}

/// OTLP retryable HTTP statuses, also used by the generic HTTP output.
/// All other unsuccessful responses, including redirects, are terminal.
pub(crate) fn status_error(
    status: reqwest::StatusCode,
    retry_after: Option<std::time::Duration>,
    error: anyhow::Error,
) -> super::batched::SendError {
    if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
        super::batched::SendError::Retryable { error, retry_after }
    } else {
        super::batched::SendError::Permanent(error)
    }
}

/// Parse both Retry-After forms. The flusher clamps hints to max_wait.
/// Invalid hints fall back to exponential backoff; past dates mean zero.
pub(crate) fn retry_after(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<std::time::Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(std::time::Duration::from_secs(seconds));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::output::batched::SendError;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn deterministic_request_build_errors_are_permanent() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .header("x-test", "invalid\nvalue")
            .build()
            .unwrap_err();
        assert!(matches!(
            request_error(error, "invalid request".to_string()),
            SendError::Permanent(_)
        ));
    }

    #[test]
    fn http_status_classification_matches_otlp() {
        for code in 300..600 {
            let error = status_error(
                reqwest::StatusCode::from_u16(code).unwrap(),
                Some(Duration::from_secs(9)),
                anyhow::anyhow!("response failed"),
            );
            if matches!(code, 429 | 502 | 503 | 504) {
                assert!(
                    matches!(error, SendError::Retryable { retry_after: Some(d), .. }
                    if d == Duration::from_secs(9)),
                    "{code}"
                );
            } else {
                assert!(matches!(error, SendError::Permanent(_)), "{code}");
            }
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let parse = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            retry_after(&headers, now)
        };
        assert_eq!(parse("25"), Some(Duration::from_secs(25)));
        assert_eq!(parse("0"), Some(Duration::ZERO));
        assert_eq!(
            parse("99999999999999999999999999"),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(
            parse(&httpdate::fmt_http_date(now + Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse(&httpdate::fmt_http_date(now - Duration::from_secs(30))),
            Some(Duration::ZERO)
        );
        for bad in ["", "soon", "-1", "+1", "1.5"] {
            assert_eq!(parse(bad), None);
        }
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new(), now), None);
    }

    fn snippet(location: &str) -> String {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::LOCATION, location.parse().unwrap());
        location_snippet(
            &headers,
            &reqwest::Url::parse("https://origin.example/v1/logs").unwrap(),
        )
    }

    #[test]
    fn location_omits_credentials_queries_and_fragments() {
        assert_eq!(
            snippet("https://user:password@collector.example:8443/logs?token=secret#private"),
            "; Location: \"https://collector.example:8443/logs\""
        );
        assert_eq!(
            snippet("../target?token=secret#private"),
            "; Location: \"https://origin.example/target\""
        );
        assert_eq!(
            snippet("//user:password@collector.example/logs?token=secret"),
            "; Location: \"https://collector.example/logs\""
        );
    }

    #[test]
    fn location_caps_long_paths_and_hides_unparseable_values() {
        let url = format!(
            "https://collector.example/{}?token=secret",
            "x".repeat(1000)
        );
        let expected: String = url.chars().take(LOCATION_MAX_CHARS).collect();
        assert_eq!(snippet(&url), format!("; Location: \"{expected}…\""));
        assert_eq!(snippet("http://[secret"), "; Location: <invalid URL>");
        assert_eq!(
            snippet("data:text/plain,secret"),
            "; Location: <non-HTTP URL>"
        );
        assert!(
            location_snippet(
                &reqwest::header::HeaderMap::new(),
                &reqwest::Url::parse("https://origin.example/").unwrap()
            )
            .is_empty()
        );
    }
}
