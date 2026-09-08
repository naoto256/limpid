//! Input modules: receive log messages from external sources.

#[cfg(feature = "journal")]
pub mod journal;
#[cfg(feature = "journal")]
mod journal_sys;
pub mod ltp;
pub mod otlp;
pub mod rate_limit;
pub mod raw_udp;
pub mod syslog_tcp;
pub mod syslog_udp;
pub mod tail;
#[cfg(windows)]
mod tail_windows;
mod udp;
#[cfg(unix)]
pub mod unix_socket;
pub(crate) mod validate;

#[cfg(windows)]
pub mod windows_event_log;
#[cfg(windows)]
mod windows_event_log_json;
#[cfg(windows)]
mod windows_event_log_state;
#[cfg(windows)]
mod windows_event_log_sys;
