//! Input modules: receive log messages from external sources.

#[cfg(feature = "journal")]
pub mod journal;
pub mod otlp_http;
pub mod rate_limit;
pub mod syslog_tcp;
pub mod syslog_tls;
pub mod syslog_udp;
pub mod tail;
pub mod unix_socket;
pub(crate) mod validate;
