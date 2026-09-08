#![cfg(windows)]

#[path = "../../src/modules/input/windows_event_log_json.rs"]
pub mod windows_event_log_json;
#[path = "../../src/modules/input/windows_event_log_state.rs"]
pub mod windows_event_log_state;

#[path = "../../src/modules/input/windows_event_log_sys.rs"]
pub mod windows_event_log_sys;
