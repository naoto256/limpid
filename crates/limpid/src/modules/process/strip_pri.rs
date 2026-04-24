//! strip_pri: removes the `<PRI>` header from event.egress.

use bytes::Bytes;

use crate::event::Event;
use crate::modules::ProcessError;

pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    if let Some(body) = strip(event.egress.as_ref()) {
        event.egress = Bytes::copy_from_slice(body);
    }
    Ok(event)
}

fn strip(msg: &[u8]) -> Option<&[u8]> {
    if msg.first() != Some(&b'<') {
        return None;
    }
    let limit = msg.len().min(6);
    let pos = msg[..limit].iter().position(|&b| b == b'>')?;
    Some(&msg[pos + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_event(raw: &str) -> Event {
        Event::new(
            Bytes::from(raw.to_string()),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn test_strips_pri() {
        let e = apply(make_event("<185>hello")).unwrap();
        assert_eq!(&*e.egress, b"hello");
    }

    #[test]
    fn test_no_pri_unchanged() {
        let e = apply(make_event("hello")).unwrap();
        assert_eq!(&*e.egress, b"hello");
    }
}
