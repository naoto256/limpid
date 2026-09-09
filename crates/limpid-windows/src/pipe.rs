//! Local-only, explicitly secured control Named Pipes.
use crate::{
    request::{MAX_FRAME, Reader, Writer},
    security::Descriptor,
};
use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    net::Shutdown,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::Mutex,
};

pub const DEFAULT_PATH: &str = r"\\.\pipe\limpid-control";
pub fn validate_path(path: &Path) -> io::Result<()> {
    let text = path.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let prefix = r"\\.\pipe\";
    if !text.to_ascii_lowercase().starts_with(prefix)
        || text.len() <= prefix.len()
        || text[prefix.len()..].contains(['\\', '/', '\0'])
        || text.encode_utf16().count() > 256
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            r"control pipe must be a local \\.\pipe\<name>",
        ));
    }
    Ok(())
}

fn create(path: &Path, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor = Descriptor::private()?;
    let mut attributes = descriptor.attributes();
    // The descriptor lives through CreateNamedPipe; the kernel copies it.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
            )
    }
}

pub struct Listener {
    path: PathBuf,
    pending: Mutex<Pending>,
}
struct Pending {
    accepting: NamedPipeServer,
    // A created instance can already receive a client before connect() is
    // polled. Keep it owned across accept cancellation, not in the future.
    spare: Option<NamedPipeServer>,
}
impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        validate_path(path)?;
        Ok(Self {
            path: path.to_owned(),
            pending: Mutex::new(Pending {
                accepting: create(path, true)?,
                spare: None,
            }),
        })
    }
    pub async fn accept(&self) -> io::Result<(Server, ())> {
        self.accept_with(|| create(&self.path, false)).await
    }

    async fn accept_with(
        &self,
        make_spare: impl FnOnce() -> io::Result<NamedPipeServer>,
    ) -> io::Result<(Server, ())> {
        let mut pending = self.pending.lock().await;
        if pending.spare.is_none() {
            // Failure leaves the current instance unconsumed and retryable.
            pending.spare = Some(make_spare()?);
        }
        pending.accepting.connect().await?;
        // No fallible operation or await after consuming the connection.
        let next = pending.spare.take().expect("spare prepared before connect");
        Ok((Server(std::mem::replace(&mut pending.accepting, next)), ()))
    }
}
pub type ServerRead = Reader<ReadHalf<NamedPipeServer>>;
pub type ServerWrite = WriteHalf<NamedPipeServer>;
pub struct Server(NamedPipeServer);
impl Server {
    pub fn into_split(self) -> (ServerRead, ServerWrite) {
        let (read, write) = tokio::io::split(self.0);
        (Reader::new(read), write)
    }
}
impl tokio::io::AsyncWrite for Server {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, bytes)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

pub struct AsyncClient;
impl AsyncClient {
    pub async fn connect(path: &Path) -> io::Result<Writer<NamedPipeClient>> {
        validate_path(path)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(Writer::new(client)),
                Err(error)
                    if error.raw_os_error() == Some(231)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct Client {
    file: File,
    ended: Cell<bool>,
    poisoned: Cell<bool>,
}
impl Client {
    pub fn connect(path: &Path) -> io::Result<Self> {
        validate_path(path)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION: connecting to
            // a planted server must not let it impersonate an admin client.
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(0x0010_0000 | 0x0001_0000)
                .open(path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        ended: Cell::new(false),
                        poisoned: Cell::new(false),
                    });
                }
                Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if how != Shutdown::Write {
            return Err(io::ErrorKind::Unsupported.into());
        }
        if self.poisoned.get() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.ended.replace(true) {
            return Ok(());
        }
        let result = (&self.file).write_all(&0u32.to_be_bytes());
        if result.is_err() {
            self.poisoned.set(true);
        }
        result
    }
}
impl Read for Client {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
impl Write for Client {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.ended.get() || self.poisoned.get() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let count = bytes.len().min(MAX_FRAME);
        if count == 0 {
            return Ok(0);
        }
        let result = self
            .file
            .write_all(&(count as u32).to_be_bytes())
            .and_then(|_| self.file.write_all(&bytes[..count]));
        if result.is_err() {
            self.poisoned.set(true);
        }
        result.map(|_| count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn name() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        PathBuf::from(format!(
            r"\\.\pipe\limpid-native-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    #[tokio::test]
    async fn first_instance_refuses_an_existing_listener() {
        let path = name();
        let _listener = Listener::bind(&path).unwrap();
        assert!(Listener::bind(&path).is_err());
    }

    #[tokio::test]
    async fn spare_creation_failure_does_not_consume_accept() {
        let path = name();
        let listener = Listener::bind(&path).unwrap();
        let client = ClientOptions::new().open(&path).unwrap();
        let failed = tokio::time::timeout(
            Duration::from_secs(1),
            listener.accept_with(|| Err(io::Error::from(io::ErrorKind::PermissionDenied))),
        )
        .await
        .expect("creation failure must precede waiting for a client");
        assert!(matches!(failed, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
        assert!(
            Listener::bind(&path).is_err(),
            "name ownership must remain held"
        );
        let (server, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        drop((client, server, listener));
        assert!(Listener::bind(&path).is_ok());
    }

    #[tokio::test]
    async fn cancelled_accept_keeps_one_spare_until_listener_drop() {
        let path = name();
        let listener = Listener::bind(&path).unwrap();
        for _ in 0..3 {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), listener.accept())
                    .await
                    .is_err()
            );
            assert!(listener.pending.lock().await.spare.is_some());
            assert!(Listener::bind(&path).is_err());
        }
        // A retained spare must be reused rather than created again.
        let client = ClientOptions::new().open(&path).unwrap();
        let (server, _) = listener
            .accept_with(|| panic!("spare must survive cancellation"))
            .await
            .unwrap();
        assert!(listener.pending.lock().await.spare.is_none());
        drop((client, server, listener));
        assert!(
            Listener::bind(&path).is_ok(),
            "drop must release all owned instances"
        );
    }

    #[tokio::test]
    async fn clients_connected_to_both_instances_survive_cancel_and_swap() {
        let path = name();
        let listener = Listener::bind(&path).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        let mut first = ClientOptions::new().open(&path).unwrap();
        let mut second = ClientOptions::new().open(&path).unwrap();
        first.write_all(b"A").await.unwrap();
        second.write_all(b"B").await.unwrap();
        let mut observed = Vec::new();
        for _ in 0..2 {
            let (mut server, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut byte = [0];
            tokio::time::timeout(Duration::from_secs(1), server.0.read_exact(&mut byte))
                .await
                .unwrap()
                .unwrap();
            observed.push(byte[0]);
        }
        observed.sort();
        assert_eq!(observed, b"AB");
        drop((first, second, listener));
        assert!(Listener::bind(&path).is_ok());
    }
    #[test]
    fn only_local_pipe_names_are_accepted() {
        assert!(validate_path(Path::new(DEFAULT_PATH)).is_ok());
        for path in [
            r"\\server\pipe\limpid",
            r"C:\control.sock",
            r"\\.\pipe\",
            r"\\.\pipe\a\b",
        ] {
            assert!(validate_path(Path::new(path)).is_err());
        }
    }
    async fn reply_once(listener: Listener) {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"inject input i\n\xff\0\n");
        writer.write_all(b"{\"injected\":1}\n").await.unwrap();
    }
    #[tokio::test]
    async fn native_async_request_end_keeps_response_readable() {
        let path = name();
        let listener = Listener::bind(&path).unwrap();
        let server = tokio::spawn(reply_once(listener));
        let mut client = AsyncClient::connect(&path).await.unwrap();
        client.write_all(b"inject input i\n\xff\0\n").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert_eq!(response, "{\"injected\":1}\n");
        server.await.unwrap();
    }
    #[tokio::test]
    async fn native_sync_request_end_keeps_response_readable() {
        let path = name();
        let listener = Listener::bind(&path).unwrap();
        let server = tokio::spawn(reply_once(listener));
        let response = tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&path).unwrap();
            client.write_all(b"inject input i\n\xff\0\n").unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response
        })
        .await
        .unwrap();
        assert_eq!(response, "{\"injected\":1}\n");
        server.await.unwrap();
    }
}
