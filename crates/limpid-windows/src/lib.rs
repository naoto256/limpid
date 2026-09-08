//! Windows boundaries shared by the daemon and its management clients.
#![cfg(windows)]
pub mod pipe;
pub mod request;
pub mod security;
