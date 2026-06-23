//! Output modules: write processed events to external destinations.

pub mod file;
pub mod http;
pub(crate) mod http_util;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod otlp;
pub(crate) mod persistent_conn;
pub mod stdout;
pub(crate) mod syslog_peers;
pub mod syslog_tcp;
pub mod syslog_udp;
pub mod unix_socket;
