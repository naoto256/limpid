//! prepend_timestamp: prepends the receive timestamp to event.egress.
//!
//! Format: `Mmm dd HH:MM:SS ` (traditional BSD syslog timestamp, local time).

use bytes::Bytes;
use chrono::Local;

use crate::event::Event;
use crate::modules::ProcessError;

pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    let ts = event
        .timestamp
        .with_timezone(&Local)
        .format("%b %e %H:%M:%S");
    let msg = String::from_utf8_lossy(&event.egress);
    event.egress = Bytes::from(format!("{} {}", ts, msg));
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn test_prepends_timestamp() {
        let e = Event::new(
            Bytes::from("hello"),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        let result = apply(e).unwrap();
        let msg = String::from_utf8_lossy(&result.egress);
        // Should end with " hello" and start with a month abbreviation
        assert!(msg.ends_with(" hello"));
        assert!(msg.len() > 16);
    }
}
