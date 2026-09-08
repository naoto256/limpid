//! Thread-confined Windows Event Log pull subscription. No DSL defaults or
//! checkpoint policy live here; the input worker must choose them explicitly.

use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::rc::Rc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, ERROR_TIMEOUT, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::EventLog::*;
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

pub enum StartPosition<'a> {
    Future,
    #[cfg(test)]
    Oldest,
    AfterBookmark(&'a str),
}

pub struct Record {
    pub xml: String,
    pub bookmark: String,
}

// The event query/subscription stays on its creating worker thread. No unsafe
// Send/Sync implementations: moving an integer Windows handle is not evidence
// that the API permits using it from arbitrary executor threads.
struct EventHandle(EVT_HANDLE, PhantomData<Rc<()>>);

impl EventHandle {
    fn checked(raw: EVT_HANDLE) -> io::Result<Self> {
        if raw == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw, PhantomData))
        }
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: each successful API result has exactly one owning wrapper.
        unsafe {
            EvtClose(self.0);
        }
    }
}

pub struct Subscription {
    // Drop the subscription before closing the event it can still signal.
    subscription: EventHandle,
    signal: OwnedHandle,
    draining: bool,
}

fn wide(value: &str) -> io::Result<Vec<u16>> {
    if value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Event Log strings cannot contain NUL",
        ));
    }
    Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
}

fn render(handle: &EventHandle, flags: u32) -> io::Result<String> {
    let mut used = 0u32;
    let mut count = 0u32;
    // SAFETY: valid owned handle; the zero-capacity probe has a null buffer.
    let ok = unsafe {
        EvtRender(
            0,
            handle.0,
            flags,
            0,
            std::ptr::null_mut(),
            &mut used,
            &mut count,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    if used == 0 || !used.is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Event Log UTF-16 buffer size",
        ));
    }
    // EvtRender sizes are bytes, not UTF-16 code units. The owned allocation
    // is aligned for UTF-16 and remains live until the synchronous call ends.
    let capacity = used;
    let mut buffer = Vec::<u16>::new();
    buffer
        .try_reserve_exact(capacity as usize / 2)
        .map_err(io::Error::other)?;
    buffer.resize(capacity as usize / 2, 0);
    let ok = unsafe {
        EvtRender(
            0,
            handle.0,
            flags,
            capacity,
            buffer.as_mut_ptr().cast(),
            &mut used,
            &mut count,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if used == 0 || used > capacity || !used.is_multiple_of(2) || buffer[used as usize / 2 - 1] != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Event Log UTF-16 result",
        ));
    }
    String::from_utf16(&buffer[..used as usize / 2 - 1])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

impl Subscription {
    pub fn open(channel: &str, query: &str, start: StartPosition<'_>) -> io::Result<Self> {
        let channel = wide(channel)?;
        let query = wide(query)?;
        let bookmark = if let StartPosition::AfterBookmark(xml) = start {
            let xml = wide(xml)?;
            // SAFETY: NUL-terminated XML lives through this synchronous call.
            Some(EventHandle::checked(unsafe {
                EvtCreateBookmark(xml.as_ptr())
            })?)
        } else {
            None
        };
        let origin = match start {
            StartPosition::Future => EvtSubscribeToFutureEvents,
            #[cfg(test)]
            StartPosition::Oldest => EvtSubscribeStartAtOldestRecord,
            StartPosition::AfterBookmark(_) => EvtSubscribeStartAfterBookmark,
        };
        // Unnamed, manual-reset event; no global namespace or ACL mutation.
        let raw_signal = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if raw_signal.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateEventW transferred one valid kernel handle to us.
        let signal = unsafe { OwnedHandle::from_raw_handle(raw_signal) };
        let subscription = EventHandle::checked(unsafe {
            EvtSubscribe(
                0,
                signal.as_raw_handle(),
                channel.as_ptr(),
                query.as_ptr(),
                bookmark.as_ref().map_or(0, |b| b.0),
                std::ptr::null(),
                None,
                origin | EvtSubscribeStrict,
            )
        })?;
        // Prime every pull subscription with EvtNext, including future-only
        // subscriptions. Waiting for the first signal without the initial
        // pull can leave newly published events unread indefinitely.
        Ok(Self {
            subscription,
            signal,
            draining: true,
        })
    }

    /// Wait for at most the caller's finite idle timeout. Stale bookmarks,
    /// permissions and log-clear errors remain native errors; no silent reset.
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Record>> {
        let started = std::time::Instant::now();
        let millis = timeout.as_millis();
        if millis >= u32::MAX as u128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Event Log wait must be finite",
            ));
        }
        if !self.draining {
            match unsafe { WaitForSingleObject(self.signal.as_raw_handle(), millis as u32) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(None),
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                _ => return Err(io::Error::other("unexpected Event Log wait result")),
            }
            // Reset before probing so an arrival concurrent with the final
            // empty read leaves a signal for the next call.
            if unsafe { ResetEvent(self.signal.as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.draining = true;
        }
        self.try_next(timeout.saturating_sub(started.elapsed()).as_millis() as u32)
    }

    fn try_next(&mut self, timeout: u32) -> io::Result<Option<Record>> {
        let mut raw = 0;
        let mut returned = 0;
        let ok = unsafe { EvtNext(self.subscription.0, 1, &mut raw, timeout, 0, &mut returned) };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error().map(|e| e as u32) {
                Some(ERROR_NO_MORE_ITEMS) => {
                    self.draining = false;
                    Ok(None)
                }
                // A signaled subscription can still be fetching its result.
                // Timeout does not mean drained; retain the notification.
                Some(ERROR_TIMEOUT) => Ok(None),
                _ => Err(error),
            };
        }
        let event = EventHandle::checked(raw)?;
        if returned != 1 {
            return Err(io::Error::other("unexpected Event Log result count"));
        }
        let xml = render(&event, EvtRenderEventXml)?;
        let bookmark = EventHandle::checked(unsafe { EvtCreateBookmark(std::ptr::null()) })?;
        if unsafe { EvtUpdateBookmark(bookmark.0, event.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Record {
            xml,
            bookmark: render(&bookmark, EvtRenderBookmark)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn future_subscription_receives_a_new_native_application_event() {
        // An unregistered classic source writes to Application. This emits
        // synthetic diagnostic records; no registry changes.
        let source = format!(
            "limpid-native-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let query = format!("*[System[Provider[@Name='{source}']]]");
        let mut subscription =
            Subscription::open("Application", &query, StartPosition::Future).unwrap();
        assert!(
            subscription
                .next(Duration::from_millis(10))
                .unwrap()
                .is_none()
        );
        let source_wide = wide(&source).unwrap();
        for phase in 0..3 {
            let expected = format!("limpid synthetic native future-subscription test {phase}");
            let marker = wide(&expected).unwrap();
            let publisher = unsafe { RegisterEventSourceW(std::ptr::null(), source_wide.as_ptr()) };
            assert!(!publisher.is_null(), "{}", io::Error::last_os_error());
            let strings = [marker.as_ptr()];
            let published = unsafe {
                ReportEventW(
                    publisher,
                    EVENTLOG_INFORMATION_TYPE,
                    0,
                    900,
                    std::ptr::null_mut(),
                    1,
                    0,
                    strings.as_ptr(),
                    std::ptr::null(),
                )
            };
            let publish_error = io::Error::last_os_error();
            unsafe {
                DeregisterEventSource(publisher);
            }
            assert_ne!(published, 0, "{publish_error}");
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut observed = false;
            while Instant::now() < deadline {
                if let Some(record) = subscription.next(Duration::from_millis(100)).unwrap() {
                    assert!(record.xml.contains(&expected));
                    observed = true;
                    break;
                }
            }
            assert!(
                observed,
                "a new published event must arrive within the native subscription budget"
            );
            assert!(
                subscription
                    .next(Duration::from_millis(10))
                    .unwrap()
                    .is_none()
            );
            assert!(
                subscription
                    .next(Duration::from_millis(10))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn reads_native_system_event_with_xml_and_resumable_bookmark() {
        let mut subscription = Subscription::open("System", "*", StartPosition::Oldest).unwrap();
        let record = subscription
            .next(Duration::from_secs(2))
            .unwrap()
            .expect("native fixture requires at least one event in the System channel");
        // Never print user event contents or persist them as a fixture.
        assert!(record.xml.starts_with("<Event"));
        assert!(record.xml.contains("<System>"));
        assert!(record.bookmark.contains("<Bookmark"));
        let expected = subscription
            .next(Duration::from_secs(2))
            .unwrap()
            .expect("native fixture requires two retained System events to prove exact resume");
        drop(subscription);
        let mut resumed = Subscription::open(
            "System",
            "*",
            StartPosition::AfterBookmark(&record.bookmark),
        )
        .unwrap();
        let next = resumed
            .next(Duration::from_secs(2))
            .unwrap()
            .expect("resume must return the retained successor record");
        assert!(
            next.bookmark == expected.bookmark,
            "resume must return the exact successor bookmark"
        );
        assert!(
            next.xml == expected.xml,
            "resume must return the exact successor event XML"
        );
    }

    #[test]
    fn idle_wait_and_close_are_bounded() {
        let mut subscription =
            Subscription::open("System", "*[System[EventID=999999]]", StartPosition::Future)
                .unwrap();
        let started = Instant::now();
        assert!(
            subscription
                .next(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        drop(subscription);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn empty_historical_query_is_idle() {
        let mut subscription =
            Subscription::open("System", "*[System[EventID=999999]]", StartPosition::Oldest)
                .unwrap();
        assert!(
            subscription
                .next(Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_channel_reports_native_error() {
        let err = Subscription::open(
            "Limpid-Nonexistent-Native-Test-Channel",
            "*",
            StartPosition::Future,
        )
        .err()
        .unwrap();
        assert_eq!(err.raw_os_error(), Some(15007));
    }

    #[test]
    fn invalid_query_reports_native_error() {
        let err = Subscription::open("System", "*[this is invalid", StartPosition::Future)
            .err()
            .unwrap();
        assert_eq!(err.raw_os_error(), Some(15001));
    }

    #[test]
    fn embedded_nul_is_rejected_without_truncating_channel() {
        let err = Subscription::open("System\0Other", "*", StartPosition::Future)
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn malformed_bookmark_does_not_fall_back_to_oldest_or_future() {
        let err = Subscription::open("System", "*", StartPosition::AfterBookmark("not XML"))
            .err()
            .unwrap();
        assert!(err.raw_os_error().is_some());
    }
}
