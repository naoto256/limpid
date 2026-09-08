//! Native synchronous stdout boundary, exercised independently of the daemon.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::IO::CancelSynchronousIo;

#[derive(Debug)]
pub struct WriteError {
    pub source: io::Error,
    pub written: usize,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stdout write failed after {} confirmed byte(s): {}",
            self.written, self.source
        )
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

struct Request {
    bytes: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    reply: oneshot::Sender<Result<(), WriteError>>,
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

pub struct Transport {
    requests: mpsc::Sender<Request>,
    serial: Arc<Mutex<()>>,
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn interrupted(written: usize) -> WriteError {
    WriteError {
        source: io::Error::new(io::ErrorKind::Interrupted, "stdout shutdown requested"),
        written,
    }
}

fn stdout_file() -> io::Result<File> {
    // SAFETY: the process stdout handle is borrowed only while duplicating it;
    // the returned File owns the duplicate, never the process's original.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "stdout has no valid Windows handle",
        ));
    }
    Ok(File::from(
        unsafe { BorrowedHandle::borrow_raw(handle) }.try_clone_to_owned()?,
    ))
}

pub fn stdout_is_regular_file() -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType};
    let file = stdout_file()?;
    // Pipe/console handles do not support file metadata queries.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Ok(false);
    }
    Ok(file.metadata()?.is_file())
}

static STDOUT: OnceLock<Arc<Transport>> = OnceLock::new();
static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Transport {
    pub fn stdout() -> io::Result<Arc<Self>> {
        let _guard = INIT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(transport) = STDOUT.get() {
            return Ok(Arc::clone(transport));
        }
        let transport = Self::from_file(stdout_file()?)?;
        let _ = STDOUT.set(Arc::clone(&transport));
        Ok(transport)
    }

    pub fn from_file(file: File) -> io::Result<Arc<Self>> {
        let (writes, receiver) = mpsc::channel::<(
            Vec<u8>,
            Arc<AtomicBool>,
            mpsc::Sender<Result<(), WriteError>>,
        )>();
        // This thread issues only stdout writes. Never cancel a Tokio blocking
        // pool thread: its next job may belong to another file or module.
        let worker = std::thread::Builder::new()
            .name("limpid-stdout".into())
            .spawn(move || {
                for (bytes, cancelled, reply) in receiver {
                    let _ = reply.send(write_all(&file, &bytes, &cancelled));
                }
            })?;
        let thread = worker.as_handle().try_clone_to_owned()?;
        drop(worker);
        let (requests, pending) = mpsc::channel::<Request>();
        // A separate native supervisor also survives Tokio runtime teardown.
        // It owns the serialization guard until the actual write completes.
        // Channel closure stops both threads after the last in-flight request.
        std::thread::Builder::new()
            .name("limpid-stdout-cancel".into())
            .spawn(move || {
                for request in pending {
                    let (reply, completion) = mpsc::channel();
                    let result = if writes
                        .send((request.bytes, Arc::clone(&request.cancelled), reply))
                        .is_err()
                    {
                        Err(WriteError {
                            source: io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "stdout worker stopped",
                            ),
                            written: 0,
                        })
                    } else {
                        loop {
                            match completion.recv_timeout(Duration::from_millis(10)) {
                                Ok(result) => break result,
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    break Err(WriteError {
                                        source: io::Error::other(
                                            "stdout worker stopped before reporting completion",
                                        ),
                                        written: 0,
                                    });
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    if request.cancelled.load(Ordering::Acquire) {
                                        // SAFETY: the owned handle identifies only our
                                        // writer, even after its thread exits. Repeat
                                        // ERROR_NOT_FOUND races until actual completion.
                                        // No next request can race a late cancellation.
                                        unsafe {
                                            CancelSynchronousIo(thread.as_raw_handle());
                                        }
                                    }
                                }
                            }
                        }
                    };
                    let _ = request.reply.send(result);
                }
            })?;
        Ok(Arc::new(Self {
            requests,
            serial: Arc::new(Mutex::new(())),
        }))
    }
    pub async fn write_frame(
        &self,
        frame: &[u8],
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), WriteError> {
        self.write_frame_mode(frame, shutdown, false).await
    }

    pub async fn write_frame_drain(
        &self,
        frame: &[u8],
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), WriteError> {
        self.write_frame_mode(frame, shutdown, true).await
    }

    async fn write_frame_mode(
        &self,
        frame: &[u8],
        shutdown: &mut watch::Receiver<bool>,
        drain: bool,
    ) -> Result<(), WriteError> {
        if !drain && *shutdown.borrow() {
            return Err(interrupted(0));
        }
        let serial = if drain {
            Arc::clone(&self.serial).lock_owned().await
        } else {
            tokio::select! {
                guard = Arc::clone(&self.serial).lock_owned() => guard,
                _ = shutdown.changed() => return Err(interrupted(0)),
            }
        };
        if !drain && *shutdown.borrow() {
            return Err(interrupted(0));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancelled));
        let (reply, mut completion) = oneshot::channel();
        let request = Request {
            bytes: frame.to_vec(),
            cancelled: Arc::clone(&cancelled),
            reply,
            _serial: serial,
        };
        self.requests.send(request).map_err(|_| WriteError {
            source: io::Error::new(io::ErrorKind::BrokenPipe, "stdout supervisor stopped"),
            written: 0,
        })?;
        let result = if drain {
            completion.await
        } else {
            tokio::select! {
                result = &mut completion => result,
                _ = shutdown.changed() => {
                    cancelled.store(true, Ordering::Release);
                    completion.await
                }
            }
        };
        result.map_err(|error| WriteError {
            source: io::Error::other(format!("stdout supervisor stopped: {error}")),
            written: 0,
        })?
    }
}

fn write_all(file: &File, bytes: &[u8], cancelled: &AtomicBool) -> Result<(), WriteError> {
    let mut written = 0;
    while written < bytes.len() {
        if cancelled.load(Ordering::Acquire) {
            return Err(interrupted(written));
        }
        // Bound individual writes so completion gives a confirmed prefix even
        // when a larger frame is interrupted by pipe backpressure.
        let remaining = &bytes[written..bytes.len().min(written + 4096)];
        let mut count = 0;
        // SAFETY: file and buffer remain owned by the worker until this
        // synchronous call returns; count is a valid DWORD output pointer.
        let ok = unsafe {
            WriteFile(
                file.as_raw_handle(),
                remaining.as_ptr(),
                remaining.len() as u32,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) {
                return Err(interrupted(written));
            }
            return Err(WriteError { source, written });
        }
        if count == 0 {
            return Err(WriteError {
                source: io::Error::new(io::ErrorKind::WriteZero, "stdout write returned zero"),
                written,
            });
        }
        written += count as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::time::Duration;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    fn pipe() -> (File, File) {
        let mut reader = std::ptr::null_mut();
        let mut writer = std::ptr::null_mut();
        // SAFETY: output pointers are valid; success transfers two handles.
        assert_ne!(
            unsafe { CreatePipe(&mut reader, &mut writer, std::ptr::null(), 4096) },
            0
        );
        unsafe {
            (
                File::from(OwnedHandle::from_raw_handle(reader)),
                File::from(OwnedHandle::from_raw_handle(writer)),
            )
        }
    }

    #[tokio::test]
    async fn redirected_file_keeps_exact_binary_frames() {
        let file = tempfile::tempfile().unwrap();
        let mut reader = file.try_clone().unwrap();
        let transport = Transport::from_file(file).unwrap();
        let (_sender, mut shutdown) = watch::channel(false);
        transport
            .write_frame(b"\xff\x00first\n", &mut shutdown)
            .await
            .unwrap();
        transport
            .write_frame(b"second\n", &mut shutdown)
            .await
            .unwrap();
        use std::io::{Seek, SeekFrom};
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"\xff\x00first\nsecond\n");
    }

    #[tokio::test]
    async fn broken_pipe_reports_zero_confirmed_bytes() {
        let (reader, writer) = pipe();
        drop(reader);
        let transport = Transport::from_file(writer).unwrap();
        let (_sender, mut shutdown) = watch::channel(false);
        let error = transport
            .write_frame(b"frame\n", &mut shutdown)
            .await
            .unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.written, 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_blocked_pipe_and_reports_confirmed_prefix() {
        let (_reader, writer) = pipe();
        let transport = Transport::from_file(writer).unwrap();
        let (sender, mut shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            transport
                .write_frame(&vec![b'x'; 1024 * 1024], &mut shutdown)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        sender.send(true).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.written, 4096);
    }

    #[tokio::test]
    async fn preexisting_shutdown_writes_nothing() {
        let file = tempfile::tempfile().unwrap();
        let observer = file.try_clone().unwrap();
        let transport = Transport::from_file(file).unwrap();
        let (_sender, mut shutdown) = watch::channel(true);
        let error = transport
            .write_frame(b"frame", &mut shutdown)
            .await
            .unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.written, 0);
        assert_eq!(observer.metadata().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn dropping_blocked_caller_does_not_cancel_the_next_frame() {
        let (mut reader, writer) = pipe();
        let transport = Transport::from_file(writer).unwrap();
        let first = Arc::clone(&transport);
        let (_sender, mut shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            first
                .write_frame(&vec![b'x'; 1024 * 1024], &mut shutdown)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Start the next request before consuming the first frame's buffered
        // prefix. Its supervisor must wait for actual cancellation completion.
        let next = Arc::clone(&transport);
        let (_sender, mut shutdown) = watch::channel(false);
        let second = tokio::spawn(async move { next.write_frame(b"next\n", &mut shutdown).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let read = std::thread::spawn(move || {
            let mut bytes = vec![0; 4101];
            reader.read_exact(&mut bytes).unwrap();
            bytes
        });
        tokio::time::timeout(Duration::from_secs(3), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let bytes = read.join().unwrap();
        assert_eq!(&bytes[..4096], vec![b'x'; 4096]);
        assert_eq!(&bytes[4096..], b"next\n");
    }

    #[tokio::test]
    async fn concurrent_frames_do_not_interleave() {
        let file = tempfile::tempfile().unwrap();
        let mut reader = file.try_clone().unwrap();
        let transport = Transport::from_file(file).unwrap();
        let mut tasks = Vec::new();
        for byte in 0..8u8 {
            let transport = Arc::clone(&transport);
            tasks.push(tokio::spawn(async move {
                let (_sender, mut shutdown) = watch::channel(false);
                transport
                    .write_frame(&vec![byte; 16384], &mut shutdown)
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        use std::io::{Seek, SeekFrom};
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 8 * 16384);
        let mut ids = Vec::new();
        for frame in bytes.chunks_exact(16384) {
            assert!(frame.iter().all(|byte| *byte == frame[0]));
            ids.push(frame[0]);
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn drain_writes_even_after_shutdown() {
        let file = tempfile::tempfile().unwrap();
        let observer = file.try_clone().unwrap();
        let transport = Transport::from_file(file).unwrap();
        let (_sender, mut shutdown) = watch::channel(true);
        transport
            .write_frame_drain(b"drained\n", &mut shutdown)
            .await
            .unwrap();
        assert_eq!(observer.metadata().unwrap().len(), 8);
    }

    #[test]
    fn runtime_shutdown_does_not_leave_a_blocked_writer() {
        let (_reader, writer) = pipe();
        let transport = Transport::from_file(writer).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let writing = Arc::clone(&transport);
        runtime.block_on(async {
            tokio::spawn(async move {
                let (_sender, mut shutdown) = watch::channel(false);
                writing
                    .write_frame(&vec![b'x'; 1024 * 1024], &mut shutdown)
                    .await
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        // Dropping the runtime drops the caller future. No executor remains to
        // drive cancellation, and no reader consumes the buffered first chunk.
        drop(runtime);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(_guard) = transport.serial.try_lock() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "writer did not report completion after its caller runtime stopped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
