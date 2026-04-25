//! Output modules: write processed events to external destinations.

pub mod file;
pub mod http;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod otlp;
pub(crate) mod persistent_conn;
pub mod stdout;
pub mod tcp;
pub mod udp;
pub mod unix_socket;
