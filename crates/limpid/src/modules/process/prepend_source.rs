//! prepend_source: prepends the source IP to event.message.

use bytes::Bytes;

use crate::event::Event;
use crate::modules::ProcessError;

pub fn apply(mut event: Event) -> Result<Event, ProcessError> {
    let ip = event.source.ip();
    let msg = String::from_utf8_lossy(&event.message);
    event.message = Bytes::from(format!("{} {}", ip, msg));
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn test_prepends_ip() {
        let e = Event::new(
            Bytes::from("hello"),
            "192.0.2.10:12345".parse::<SocketAddr>().unwrap(),
        );
        let result = apply(e).unwrap();
        assert_eq!(&*result.message, b"192.0.2.10 hello");
    }
}
