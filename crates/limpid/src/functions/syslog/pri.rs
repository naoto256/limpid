//! Shared `<PRI>` header parser for the `syslog.*` primitives.
//!
//! RFC 5424 §6.2.1 defines PRI as `<N>` where `N` is 1–3 digits and the
//! numeric value is 0..=191 (`facility * 8 + severity`). `strip_pri`,
//! `extract_pri`, and `set_pri` all need to recognise the same header,
//! so the scan lives here once — update this one function if the spec
//! ever grows (e.g. extended PRI).

/// Parse a leading `<PRI>` header.
///
/// Returns `(pri_value, body_offset)` when `s` begins with a valid
/// header: `<` + 1–3 ASCII digits + `>`, value ≤ 191. The caller can
/// recover the body with `&s[body_offset..]`. Returns `None` otherwise
/// (no allocation, no panic).
pub(crate) fn parse_leading_pri(s: &str) -> Option<(u16, usize)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    // PRI is at most 3 digits, so `<` + 3 digits + `>` = 5 bytes; scan a
    // little further to keep the loop branch-free on the common case.
    let limit = bytes.len().min(6);
    let gt_pos = bytes[..limit].iter().position(|&b| b == b'>')?;
    if gt_pos < 2 {
        return None;
    }
    let digits = &bytes[1..gt_pos];
    if !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    if n > 191 {
        return None;
    }
    Some((n, gt_pos + 1))
}
