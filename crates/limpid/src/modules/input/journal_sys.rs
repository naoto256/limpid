//! Minimal FFI bindings to libsystemd's `sd_journal_*` reader API.
//!
//! Hand-written declarations of the ~11 `sd_journal_*` functions the
//! `journal` input actually calls (see `sd-journal(3)`) — not a
//! general-purpose systemd binding. A mechanical translation of a
//! public C API's parameter shapes carries no expressive content of
//! its own, so this module is MIT OR Apache-2.0 even though it
//! dynamically links `libsystemd` (LGPL-2.1-or-later WITH
//! GCC-exception-2.0) at build time via `build.rs`; the
//! distro-provided `libsystemd.so.0` this links against is covered by
//! the LGPL §6(b) dynamic-linking safe harbour. Replaces the
//! `rust-systemd`/`libsystemd-sys` dependency this crate previously
//! carried under a `deny.toml` license exception.
//!
//! `sd_journal` handles are not thread-safe — "only a single thread
//! may operate on a given object at any given time" (`sd-journal(3)`).
//! [`Journal`] is `!Send`/`!Sync` (it holds a raw pointer), so it
//! can't silently cross threads; callers must create and drop it on
//! the same thread, matching how the journal input's blocking reader
//! already uses it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use anyhow::{Result, bail};

/// Opaque `sd_journal` handle. Only ever touched behind a pointer —
/// its layout is libsystemd's business, not ours.
#[repr(C)]
struct SdJournal {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn sd_journal_open(ret: *mut *mut SdJournal, flags: c_int) -> c_int;
    fn sd_journal_close(j: *mut SdJournal);
    fn sd_journal_next(j: *mut SdJournal) -> c_int;
    fn sd_journal_previous(j: *mut SdJournal) -> c_int;
    fn sd_journal_add_match(j: *mut SdJournal, data: *const c_void, size: usize) -> c_int;
    fn sd_journal_seek_tail(j: *mut SdJournal) -> c_int;
    fn sd_journal_seek_head(j: *mut SdJournal) -> c_int;
    fn sd_journal_seek_cursor(j: *mut SdJournal, cursor: *const c_char) -> c_int;
    fn sd_journal_get_cursor(j: *mut SdJournal, cursor: *mut *mut c_char) -> c_int;
    fn sd_journal_get_realtime_usec(j: *mut SdJournal, ret: *mut u64) -> c_int;
    fn sd_journal_get_monotonic_usec(
        j: *mut SdJournal,
        ret: *mut u64,
        ret_boot_id: *mut Id128,
    ) -> c_int;
    fn sd_journal_restart_data(j: *mut SdJournal);
    fn sd_journal_enumerate_data(
        j: *mut SdJournal,
        data: *mut *const c_void,
        length: *mut usize,
    ) -> c_int;
}

/// `sd_id128_t` — a 16-byte boot/machine identifier. Only its size and
/// alignment matter here: `Journal::monotonic_timestamp` discards the
/// value, matching how the sole caller already ignores it.
#[repr(C)]
struct Id128 {
    _bytes: [u8; 16],
}

/// One field ("NAME=value") from the current journal entry.
pub struct JournalField<'a> {
    name: &'a [u8],
    value: Option<&'a [u8]>,
}

impl<'a> JournalField<'a> {
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    pub fn value(&self) -> Option<&'a [u8]> {
        self.value
    }
}

pub struct Journal {
    ptr: *mut SdJournal,
}

impl Journal {
    /// Opens the local journal with no restricting flags (`flags =
    /// 0`) — equivalent to the prior
    /// `systemd::journal::OpenOptions::default().open()`, whose every
    /// option defaulted to `false`.
    pub fn open() -> Result<Self> {
        let mut ptr: *mut SdJournal = ptr::null_mut();
        let rc = unsafe { sd_journal_open(&mut ptr, 0) };
        if rc < 0 {
            bail!("sd_journal_open failed: {}", errno_msg(rc));
        }
        Ok(Self { ptr })
    }

    /// Adds a `key=value` match filter (`sd_journal_add_match`, which
    /// takes the field as one `key=value` byte blob, not separate
    /// name/value arguments).
    pub fn match_add(&mut self, key: &str, val: &str) -> Result<()> {
        let filter = format!("{key}={val}");
        let rc = unsafe { sd_journal_add_match(self.ptr, filter.as_ptr().cast(), filter.len()) };
        if rc < 0 {
            bail!("sd_journal_add_match failed: {}", errno_msg(rc));
        }
        Ok(())
    }

    pub fn seek_cursor(&mut self, cursor: &str) -> Result<()> {
        let c = CString::new(cursor)?;
        let rc = unsafe { sd_journal_seek_cursor(self.ptr, c.as_ptr()) };
        if rc < 0 {
            bail!("sd_journal_seek_cursor failed: {}", errno_msg(rc));
        }
        Ok(())
    }

    pub fn seek_tail(&mut self) -> Result<()> {
        let rc = unsafe { sd_journal_seek_tail(self.ptr) };
        if rc < 0 {
            bail!("sd_journal_seek_tail failed: {}", errno_msg(rc));
        }
        Ok(())
    }

    /// Position the read pointer before the first (matching) journal
    /// entry. Used by the first-start anchoring fallback when the
    /// active `match` view has no past entries — see
    /// `run_journal_reader::anchor_at_tail_or_head` for the seek
    /// semantics rationale.
    pub fn seek_head(&mut self) -> Result<()> {
        let rc = unsafe { sd_journal_seek_head(self.ptr) };
        if rc < 0 {
            bail!("sd_journal_seek_head failed: {}", errno_msg(rc));
        }
        Ok(())
    }

    /// `Ok(n)` mirrors `sd_journal_next`'s own contract: `n > 0` means
    /// the read pointer advanced, `0` means end of journal.
    pub fn next(&mut self) -> Result<c_int> {
        let rc = unsafe { sd_journal_next(self.ptr) };
        if rc < 0 {
            bail!("sd_journal_next failed: {}", errno_msg(rc));
        }
        Ok(rc)
    }

    pub fn previous(&mut self) -> Result<c_int> {
        let rc = unsafe { sd_journal_previous(self.ptr) };
        if rc < 0 {
            bail!("sd_journal_previous failed: {}", errno_msg(rc));
        }
        Ok(rc)
    }

    pub fn restart_data(&mut self) {
        unsafe { sd_journal_restart_data(self.ptr) };
    }

    /// Reads the next field of the current entry. libsystemd returns
    /// each field as one raw `NAME=value` blob; this splits it on the
    /// first `=` the same way the prior crate's field type did.
    pub fn enumerate_data(&mut self) -> Result<Option<JournalField<'_>>> {
        let mut data: *const c_void = ptr::null();
        let mut len: usize = 0;
        let rc = unsafe { sd_journal_enumerate_data(self.ptr, &mut data, &mut len) };
        if rc < 0 {
            bail!("sd_journal_enumerate_data failed: {}", errno_msg(rc));
        }
        if rc == 0 {
            return Ok(None);
        }
        // `rc > 0` means libsystemd populated (data, len) with a valid
        // buffer, so `data` is non-null under the sd-journal(3) contract.
        // Guard anyway: `slice::from_raw_parts` is UB on a null pointer
        // even when `len == 0`, and treating a contract violation as
        // "no more fields" degrades gracefully instead of invoking UB.
        if data.is_null() {
            return Ok(None);
        }
        let blob = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
        let (name, value) = match blob.iter().position(|&b| b == b'=') {
            Some(idx) => (&blob[..idx], Some(&blob[idx + 1..])),
            None => (blob, None),
        };
        Ok(Some(JournalField { name, value }))
    }

    pub fn cursor(&mut self) -> Result<String> {
        let mut out: *mut c_char = ptr::null_mut();
        let rc = unsafe { sd_journal_get_cursor(self.ptr, &mut out) };
        if rc < 0 || out.is_null() {
            bail!("sd_journal_get_cursor failed: {}", errno_msg(rc));
        }
        let cursor = unsafe { CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        // sd_journal_get_cursor(3): the returned string is allocated
        // via malloc(3) and the caller must free(3) it.
        unsafe { libc::free(out.cast()) };
        Ok(cursor)
    }

    pub fn timestamp_usec(&mut self) -> Result<u64> {
        let mut usec: u64 = 0;
        let rc = unsafe { sd_journal_get_realtime_usec(self.ptr, &mut usec) };
        if rc < 0 {
            bail!("sd_journal_get_realtime_usec failed: {}", errno_msg(rc));
        }
        Ok(usec)
    }

    /// Returns `(monotonic_usec, boot_id)`. The boot id is opaque and
    /// unused — the sole caller already discards it (`_boot_id`) —
    /// but `sd_journal_get_monotonic_usec` requires a valid out
    /// pointer, so it's read into a local and dropped.
    pub fn monotonic_timestamp(&mut self) -> Result<(u64, ())> {
        let mut usec: u64 = 0;
        let mut boot_id = Id128 { _bytes: [0; 16] };
        let rc = unsafe { sd_journal_get_monotonic_usec(self.ptr, &mut usec, &mut boot_id) };
        if rc < 0 {
            bail!("sd_journal_get_monotonic_usec failed: {}", errno_msg(rc));
        }
        Ok((usec, ()))
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        unsafe { sd_journal_close(self.ptr) };
    }
}

/// `sd_journal_*` functions return `0`/positive on success or a
/// negative errno-style code on failure (`sd-journal(3)`).
fn errno_msg(rc: c_int) -> String {
    std::io::Error::from_raw_os_error(-rc).to_string()
}
