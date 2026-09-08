//! Windows SCM host. Readiness follows Runtime::start; STOP/SHUTDOWN use the
//! existing graceful shutdown, and PARAMCHANGE uses the existing reload path.
use crate::signal::SignalAction;
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU32, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc};
use windows_sys::Win32::System::Services::*;

static CONFIG: OnceLock<String> = OnceLock::new();
static SEND: OnceLock<mpsc::UnboundedSender<SignalAction>> = OnceLock::new();
static RECEIVE: OnceLock<Mutex<mpsc::UnboundedReceiver<SignalAction>>> = OnceLock::new();
static HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static STATE: AtomicU32 = AtomicU32::new(SERVICE_START_PENDING);
const NAME: &[u16] = &[108, 105, 109, 112, 105, 100, 0];

pub fn data_directory() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into()))
        .join("limpid")
}
pub fn active() -> bool {
    CONFIG.get().is_some()
}

fn publish(state: u32, failed: bool) -> Result<()> {
    let handle = HANDLE.load(Ordering::Acquire);
    if handle.is_null() {
        anyhow::bail!("SCM status handle is not registered");
    }
    let pending = matches!(state, SERVICE_START_PENDING | SERVICE_STOP_PENDING);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_PARAMCHANGE
        } else {
            0
        },
        dwWin32ExitCode: if failed { 1066 } else { 0 },
        dwServiceSpecificExitCode: u32::from(failed),
        dwCheckPoint: u32::from(pending),
        dwWaitHint: if pending { 30000 } else { 0 },
    };
    if unsafe { SetServiceStatus(handle, &status) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    STATE.store(state, Ordering::Release);
    Ok(())
}

pub fn running() -> Result<()> {
    if active() {
        publish(SERVICE_RUNNING, false)?;
    }
    Ok(())
}
pub async fn next_action() -> Result<SignalAction> {
    RECEIVE
        .get()
        .context("SCM action channel is not initialized")?
        .lock()
        .await
        .recv()
        .await
        .context("SCM action channel closed")
}

unsafe extern "system" fn handler(
    control: u32,
    _: u32,
    _: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) -> u32 {
    let action = match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            let _ = publish(SERVICE_STOP_PENDING, false);
            Some(SignalAction::Shutdown)
        }
        SERVICE_CONTROL_PARAMCHANGE => Some(SignalAction::Reload),
        SERVICE_CONTROL_INTERROGATE => {
            let _ = publish(STATE.load(Ordering::Acquire), false);
            None
        }
        _ => return 120,
    };
    if let Some(action) = action
        && SEND.get().is_none_or(|sender| sender.send(action).is_err())
    {
        return 1062;
    }
    0
}

unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(NAME.as_ptr(), Some(handler), std::ptr::null()) };
    if handle.is_null() {
        tracing::error!(
            "SCM handler registration failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    HANDLE.store(handle, Ordering::Release);
    if let Err(error) = publish(SERVICE_START_PENDING, false) {
        tracing::error!("SCM startup status failed: {error}");
        return;
    }
    let result = std::panic::catch_unwind(|| {
        crate::run_daemon(
            CONFIG
                .get()
                .expect("SCM config initialized before dispatch"),
        )
    });
    let failed = match result {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            tracing::error!("service stopped with an error: {error:#}");
            true
        }
        Err(_) => {
            tracing::error!("service panicked");
            true
        }
    };
    if let Err(error) = publish(SERVICE_STOPPED, failed) {
        tracing::error!("SCM final status failed: {error}");
    }
}

pub fn run(config: &str, debug: bool) -> Result<()> {
    // The installer creates and ACLs this directory. Refuse an absent log
    // directory instead of silently losing all service diagnostics to stderr.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_directory().join("log").join("daemon.log"))
        .context("cannot open service diagnostic log")?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(if debug { "limpid=trace" } else { "limpid=info" })
        .with_writer(std::sync::Mutex::new(log))
        .try_init()
        .map_err(|e| anyhow::anyhow!("service logging: {e}"))?;
    CONFIG
        .set(config.to_owned())
        .map_err(|_| anyhow::anyhow!("SCM host already initialized"))?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let _ = SEND.set(sender);
    let _ = RECEIVE.set(Mutex::new(receiver));
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: NAME.as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("--service must be launched by the Windows Service Control Manager");
    }
    Ok(())
}
