//! parse_syslog: parses RFC 3164 (BSD) and RFC 5424 syslog headers into event fields.
//!
//! Auto-detects format based on the byte after `<PRI>`:
//! - Digit 1-9 followed by SP → RFC 5424 (versioned)
//! - Otherwise → RFC 3164 (BSD traditional)
//!
//! Extracted fields:
//!   fields.hostname     — originating host
//!   fields.appname      — application name (RFC 5424) or tag (RFC 3164)
//!   fields.procid       — process ID (if present)
//!   fields.msgid        — message ID (RFC 5424 only)
//!   fields.syslog_msg   — message body after header
//!
//! Also sets `message` to the parsed message body.

use serde_json::Value;

use crate::event::Event;
use crate::modules::ProcessError;

pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    let raw = String::from_utf8_lossy(&event.raw).into_owned();

    // Skip PRI: <PRI>rest
    let after_pri =
        skip_pri(&raw).ok_or_else(|| ProcessError::Failed("no PRI header found".into()))?;

    // Detect RFC version: RFC 5424 starts with VERSION SP (e.g. "1 ")
    if after_pri.len() >= 2
        && after_pri.as_bytes()[0].is_ascii_digit()
        && after_pri.as_bytes()[1] == b' '
    {
        parse_rfc5424(after_pri, &mut event)
    } else {
        parse_rfc3164(after_pri, &mut event)
    }

    Ok(event)
}

/// Skip `<PRI>` and return the rest of the message.
fn skip_pri(raw: &str) -> Option<&str> {
    if !raw.starts_with('<') {
        return None;
    }
    let end = raw.find('>')?;
    Some(&raw[end + 1..])
}

/// Parse RFC 5424:
/// `VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID SP [SD] MSG`
fn parse_rfc5424(input: &str, event: &mut Event) {
    let mut parts = input.splitn(7, ' ');

    let _version = parts.next(); // already validated
    let _timestamp = parts.next();
    let hostname = parts.next().unwrap_or("-");
    let appname = parts.next().unwrap_or("-");
    let procid = parts.next().unwrap_or("-");
    let msgid = parts.next().unwrap_or("-");
    let remainder = parts.next().unwrap_or("");

    // Skip structured data [xxx] to get to message
    let msg = skip_structured_data(remainder);

    set_field(event, "hostname", hostname);
    set_field(event, "appname", appname);
    if procid != "-" {
        set_field(event, "procid", procid);
    }
    if msgid != "-" {
        set_field(event, "msgid", msgid);
    }
    if !msg.is_empty() {
        event
            .fields
            .insert("syslog_msg".into(), Value::String(msg.to_string()));
        event.message = bytes::Bytes::from(msg.to_string());
    }
}

/// Parse RFC 3164:
/// `TIMESTAMP SP HOSTNAME SP TAG: MSG`
/// or `TIMESTAMP SP HOSTNAME SP TAG[PID]: MSG`
fn parse_rfc3164(input: &str, event: &mut Event) {
    // Timestamp: "Apr 15 10:30:00" (15 chars) or similar
    // Try to find hostname after timestamp by skipping to the 3rd space-separated token
    let mut rest = input;

    // Skip timestamp: consume tokens until we've passed the time part
    // RFC 3164 timestamp: "Mon DD HH:MM:SS" — 3 space-separated parts
    if let Some(idx) = nth_space(rest, 3) {
        rest = &rest[idx..];
    }

    // Next token is hostname
    let (hostname, after_host) = next_token(rest);

    // Next is TAG[:] or TAG[PID][:] MSG
    let tag_with_msg = after_host;

    // Parse TAG[PID]: MSG
    let (appname, procid, msg) = parse_tag_and_msg(tag_with_msg);

    if !hostname.is_empty() {
        set_field(event, "hostname", hostname);
    }
    if !appname.is_empty() {
        set_field(event, "appname", appname);
    }
    if let Some(pid) = procid {
        set_field(event, "procid", pid);
    }
    if !msg.is_empty() {
        event
            .fields
            .insert("syslog_msg".into(), Value::String(msg.to_string()));
        event.message = bytes::Bytes::from(msg.to_string());
    }
}

/// Parse "TAG[PID]: MSG" or "TAG: MSG" or "TAG MSG"
fn parse_tag_and_msg(input: &str) -> (&str, Option<&str>, &str) {
    let input = input.trim_start();

    // Find the colon+space separator
    if let Some(colon_pos) = input.find(": ") {
        let tag_part = &input[..colon_pos];
        let msg = &input[colon_pos + 2..];

        // Check for [PID] in tag
        if let Some(bracket_start) = tag_part.find('[')
            && let Some(bracket_end) = tag_part.find(']')
        {
            let appname = &tag_part[..bracket_start];
            let procid = &tag_part[bracket_start + 1..bracket_end];
            return (appname, Some(procid), msg);
        }

        return (tag_part, None, msg);
    }

    // No colon — treat entire input as message
    ("", None, input)
}

fn skip_structured_data(input: &str) -> &str {
    let input = input.trim_start();
    if input.starts_with('[') {
        // Skip all [xxx] blocks, handling escaped \] within SD values
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // skip escaped char (e.g. \] or \\)
                } else if bytes[i] == b']' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            // Skip whitespace between SD elements
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
        }
        &input[i.min(input.len())..]
    } else if let Some(rest) = input.strip_prefix('-') {
        // NILVALUE for structured data
        rest.trim_start()
    } else {
        input
    }
}

fn set_field(event: &mut Event, key: &str, value: &str) {
    if value != "-" && !value.is_empty() {
        event.fields.insert(key.into(), Value::String(value.into()));
    }
}

fn next_token(input: &str) -> (&str, &str) {
    let input = input.trim_start();
    match input.find(' ') {
        Some(pos) => (&input[..pos], &input[pos + 1..]),
        None => (input, ""),
    }
}

fn nth_space(input: &str, n: usize) -> Option<usize> {
    let mut count = 0;
    for (i, b) in input.bytes().enumerate() {
        if b == b' ' {
            count += 1;
            if count == n {
                return Some(i + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_event(raw: &str) -> Event {
        Event::new(
            Bytes::from(raw.to_string()),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn test_rfc5424_basic() {
        let event =
            make_event("<134>1 2026-04-15T10:30:00Z firewall01 sshd 1234 - - Failed password");
        let result = apply(event).unwrap();
        assert_eq!(
            result.fields["hostname"],
            Value::String("firewall01".into())
        );
        assert_eq!(result.fields["appname"], Value::String("sshd".into()));
        assert_eq!(result.fields["procid"], Value::String("1234".into()));
        assert_eq!(
            result.fields["syslog_msg"],
            Value::String("Failed password".into())
        );
    }

    #[test]
    fn test_rfc5424_with_structured_data() {
        let event = make_event(
            "<134>1 2026-04-15T10:30:00Z host app 999 ID1 [meta src=\"10.0.0.1\"] Hello world",
        );
        let result = apply(event).unwrap();
        assert_eq!(result.fields["hostname"], Value::String("host".into()));
        assert_eq!(result.fields["appname"], Value::String("app".into()));
        assert_eq!(result.fields["msgid"], Value::String("ID1".into()));
        assert_eq!(
            result.fields["syslog_msg"],
            Value::String("Hello world".into())
        );
    }

    #[test]
    fn test_rfc3164_with_pid() {
        let event = make_event("<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password for root");
        let result = apply(event).unwrap();
        assert_eq!(result.fields["hostname"], Value::String("myhost".into()));
        assert_eq!(result.fields["appname"], Value::String("sshd".into()));
        assert_eq!(result.fields["procid"], Value::String("1234".into()));
        assert_eq!(
            result.fields["syslog_msg"],
            Value::String("Failed password for root".into())
        );
    }

    #[test]
    fn test_rfc3164_without_pid() {
        let event = make_event("<134>Apr 15 10:30:00 myhost kernel: Out of memory");
        let result = apply(event).unwrap();
        assert_eq!(result.fields["hostname"], Value::String("myhost".into()));
        assert_eq!(result.fields["appname"], Value::String("kernel".into()));
        assert_eq!(
            result.fields["syslog_msg"],
            Value::String("Out of memory".into())
        );
    }

    #[test]
    fn test_no_pri_fails() {
        let event = make_event("no pri header here");
        assert!(apply(event).is_err());
    }
}
