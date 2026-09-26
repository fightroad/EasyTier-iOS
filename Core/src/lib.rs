use std::{
    ffi::{c_char, c_int, CStr, CString},
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use easytier::{common::config::TomlConfigLoader, instance_manager::NetworkInstanceManager};
use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::runtime::Runtime;
use tracing_oslog::OsLogger;
use tracing_subscriber::layer::SubscriberExt as _;
use uuid::Uuid;

type SharedLogFile = Arc<Mutex<File>>;
type InstanceCallback = Option<extern "C" fn(*const c_char)>;

struct CoreContext {
    runtime: Runtime,
}

impl CoreContext {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("EasyTier runtime");
        Self { runtime }
    }
}

// Every VPN session owns its manager and one lifecycle task, irrespective of
// where its configuration comes from. No late callbacks can reach a new manager.
struct ManagedInstance {
    coordinator: Arc<InstanceCoordinator>,
    monitor: tokio::task::JoinHandle<()>,
}

impl ManagedInstance {
    fn new(coordinator: Arc<InstanceCoordinator>) -> Self {
        let monitor = CONTEXT.runtime.spawn(coordinator.clone().monitor());
        Self {
            coordinator,
            monitor,
        }
    }

    fn stop(self) -> Result<(), String> {
        self.coordinator.stop();
        self.monitor.abort();
        let manager = &self.coordinator.manager;
        manager
            .delete_network_instance(manager.list_network_instance_ids())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

enum RunMode {
    Idle,
    Local(ManagedInstance),
    Web(ManagedWebClient),
}

impl RunMode {
    fn instance(&self) -> Result<&InstanceCoordinator, String> {
        match self {
            Self::Idle => Err("tunnel session is not active".into()),
            Self::Local(instance) => Ok(&instance.coordinator),
            Self::Web(web) => Ok(&web.instance.coordinator),
        }
    }
}

mod instance;
mod web;
use instance::InstanceCoordinator;
#[cfg(test)]
use web::normalize_config_server_endpoint;
use web::ManagedWebClient;

static CONTEXT: Lazy<CoreContext> = Lazy::new(CoreContext::new);
static MODE: Lazy<Mutex<RunMode>> = Lazy::new(|| Mutex::new(RunMode::Idle));
static LOGGER_FILE: Lazy<Mutex<Option<SharedLogFile>>> = Lazy::new(|| Mutex::new(None));
static LOGGER_READY: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

/// Default max size for `easytier.log` when caller passes 0 for "use default".
const DEFAULT_MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
struct SharedLogWriter {
    file: SharedLogFile,
    max_bytes: u64,
}
struct SharedLogWriteGuard {
    file: SharedLogFile,
    max_bytes: u64,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
    type Writer = SharedLogWriteGuard;
    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriteGuard {
            file: self.file.clone(),
            max_bytes: self.max_bytes,
        }
    }
}

fn maybe_rotate_log_file(file: &mut File, max_bytes: u64, upcoming_len: usize) -> io::Result<()> {
    // `max_bytes == 0` should not appear after init_logger remaps it to the default.
    if max_bytes == 0 {
        return Ok(());
    }
    let pos = file.stream_position()?;
    // Only truncate when the file already has content and the next write would exceed the cap.
    // A single oversized record may temporarily exceed the cap; the next write will truncate.
    if pos > 0 && pos.saturating_add(upcoming_len as u64) > max_bytes {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"--- log truncated: size limit reached ---\n")?;
    }
    Ok(())
}

impl Write for SharedLogWriteGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self.file.lock().map_err(lock_io_error)?;
        maybe_rotate_log_file(&mut file, self.max_bytes, buf.len())?;
        file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.lock().map_err(lock_io_error)?.flush()
    }
}

fn lock_io_error<T>(error: std::sync::PoisonError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

fn set_error(out: *mut *const c_char, message: impl ToString) {
    if out.is_null() {
        return;
    }
    if let Ok(message) = CString::new(message.to_string()) {
        unsafe {
            *out = message.into_raw();
        }
    }
}

fn ffi_result(result: Result<(), String>, err_msg: *mut *const c_char) -> c_int {
    match result {
        Ok(()) => 0,
        Err(error) => {
            set_error(err_msg, error);
            -1
        }
    }
}

fn active_instance() -> Result<(Arc<NetworkInstanceManager>, Uuid), String> {
    let mode = MODE.lock().map_err(|error| error.to_string())?;
    let instance = mode.instance()?;
    let id = instance.current_id().ok_or("no running instance")?;
    Ok((instance.manager.clone(), id))
}

#[no_mangle]
/// # Safety
/// Initialize logger.
/// `max_bytes`: 0 means use [`DEFAULT_MAX_LOG_BYTES`].
/// `enable_file_log`: non-zero writes `easytier.log`; zero keeps OSLog only.
pub extern "C" fn init_logger(
    path: *const c_char,
    level: *const c_char,
    subsystem: *const c_char,
    max_bytes: u64,
    enable_file_log: c_int,
    err_msg: *mut *const c_char,
) -> c_int {
    let result = (|| -> Result<(), String> {
        if path.is_null() || level.is_null() || subsystem.is_null() {
            return Err("logger argument is null".to_string());
        }
        let mut ready = LOGGER_READY.lock().map_err(|error| error.to_string())?;
        if *ready {
            return Ok(());
        }
        let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
        let level = unsafe { CStr::from_ptr(level) }.to_string_lossy();
        let subsystem = unsafe { CStr::from_ptr(subsystem) }.to_string_lossy();
        let max_bytes = if max_bytes == 0 {
            DEFAULT_MAX_LOG_BYTES
        } else {
            max_bytes
        };
        let enable_file_log = enable_file_log != 0;
        let filter = tracing_subscriber::EnvFilter::new(level.as_ref());
        if enable_file_log {
            let file = Arc::new(Mutex::new(
                File::create(path.as_ref()).map_err(|error| error.to_string())?,
            ));
            let collector = tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(SharedLogWriter {
                            file: file.clone(),
                            max_bytes,
                        })
                        .with_ansi(false),
                )
                .with(OsLogger::new(subsystem.as_ref(), "rust"));
            tracing::subscriber::set_global_default(collector)
                .map_err(|error| error.to_string())?;
            *LOGGER_FILE.lock().map_err(|error| error.to_string())? = Some(file);
        } else {
            let collector = tracing_subscriber::registry()
                .with(filter)
                .with(OsLogger::new(subsystem.as_ref(), "rust"));
            tracing::subscriber::set_global_default(collector)
                .map_err(|error| error.to_string())?;
        }
        *ready = true;
        Ok(())
    })();
    ffi_result(result, err_msg)
}

#[no_mangle]
/// # Safety
/// Clear the currently initialized file logger and reset its file offset.
/// No-op (success) when file logging was never enabled.
pub extern "C" fn clear_logger(err_msg: *mut *const c_char) -> c_int {
    ffi_result(
        (|| {
            let file = match LOGGER_FILE
                .lock()
                .map_err(|error| error.to_string())?
                .clone()
            {
                Some(file) => file,
                None => return Ok(()),
            };
            let mut file = file.lock().map_err(|error| error.to_string())?;
            file.set_len(0).map_err(|error| error.to_string())?;
            file.seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            file.flush().map_err(|error| error.to_string())
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn free_string(value: *const c_char) {
    if !value.is_null() {
        unsafe {
            drop(CString::from_raw(value as *mut c_char));
        }
    }
}

#[no_mangle]
pub extern "C" fn run_network_instance(
    cfg_str: *const c_char,
    callback: InstanceCallback,
    err_msg: *mut *const c_char,
) -> c_int {
    ffi_result(
        (|| {
            if callback.is_none() {
                return Err("instance callback is required".into());
            }
            if cfg_str.is_null() {
                return Err("cfg_str is null".to_string());
            }
            let config = unsafe { CStr::from_ptr(cfg_str) }.to_string_lossy();
            let config =
                TomlConfigLoader::new_from_str(&config).map_err(|error| error.to_string())?;
            let mut mode = MODE.lock().map_err(|error| error.to_string())?;
            if !matches!(*mode, RunMode::Idle) {
                return Err("another EasyTier mode is already running".to_string());
            }
            let coordinator = Arc::new(InstanceCoordinator::new(
                Arc::new(NetworkInstanceManager::new()),
                callback,
            ));
            CONTEXT.runtime.block_on(coordinator.start_local(config))?;
            *mode = RunMode::Local(ManagedInstance::new(coordinator));
            Ok(())
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn start_config_server_client(
    url: *const c_char,
    hostname: *const c_char,
    machine_id: *const c_char,
    secure_mode: bool,
    callback: InstanceCallback,
    err_msg: *mut *const c_char,
) -> c_int {
    ffi_result(
        (|| {
            if callback.is_none() {
                return Err("config server callback is required".into());
            }
            if url.is_null() || machine_id.is_null() {
                return Err("config server URL and machine ID are required".to_string());
            }
            let url = unsafe { CStr::from_ptr(url) }
                .to_string_lossy()
                .trim()
                .to_string();
            let machine_id = unsafe { CStr::from_ptr(machine_id) }
                .to_string_lossy()
                .trim()
                .to_string();
            let hostname = if hostname.is_null() {
                None
            } else {
                let value = unsafe { CStr::from_ptr(hostname) }
                    .to_string_lossy()
                    .trim()
                    .to_string();
                (!value.is_empty()).then_some(value)
            };
            if machine_id.is_empty() {
                return Err("machine ID is empty".to_string());
            }
            let mut mode = MODE.lock().map_err(|error| error.to_string())?;
            if !matches!(*mode, RunMode::Idle) {
                return Err("another EasyTier mode is already running".to_string());
            }
            *mode = RunMode::Web(ManagedWebClient::start(
                &url,
                machine_id,
                hostname,
                secure_mode,
                callback,
            )?);
            Ok(())
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn is_config_server_client_connected() -> c_int {
    MODE.lock()
        .ok()
        .and_then(|mode| match &*mode {
            RunMode::Web(managed) => Some(managed.is_connected()),
            _ => None,
        })
        .map(i32::from)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn complete_instance_setup(
    generation: u64,
    success: bool,
    error: *const c_char,
) -> c_int {
    let result = (|| {
        let message = if error.is_null() {
            "instance setup failed".to_string()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        let mode = MODE.lock().map_err(|error| error.to_string())?;
        mode.instance()?
            .complete_setup(generation, if success { Ok(()) } else { Err(message) })
    })();
    if let Err(error) = result {
        tracing::warn!(%error, generation, "setup acknowledgement rejected");
        return -1;
    }
    0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct TunnelOptions {
    ipv4: Option<String>,
    ipv6: Option<String>,
    mtu: Option<u32>,
    routes: Vec<String>,
    #[serde(rename = "magicDNS")]
    magic_dns: bool,
    dns: Vec<String>,
}

#[derive(Serialize)]
struct InstanceStatus {
    status: &'static str,
    #[serde(rename = "serverConnected")]
    server_connected: bool,
    #[serde(rename = "instanceID")]
    instance_id: Option<String>,
    #[serde(rename = "instanceName")]
    instance_name: Option<String>,
    #[serde(rename = "networkName")]
    network_name: Option<String>,
    generation: u64,
    error: Option<String>,
    options: Option<TunnelOptions>,
}

#[no_mangle]
pub extern "C" fn get_config_server_status(
    json: *mut *const c_char,
    err_msg: *mut *const c_char,
) -> c_int {
    ffi_result(
        (|| {
            if json.is_null() {
                return Err("json is null".to_string());
            }
            let mode = MODE.lock().map_err(|error| error.to_string())?;
            let RunMode::Web(managed) = &*mode else {
                return Err("config server client is not running".to_string());
            };
            let status = managed.status()?;
            let value =
                CString::new(serde_json::to_string(&status).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            unsafe {
                *json = value.into_raw();
            }
            Ok(())
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn get_instance_status(
    json: *mut *const c_char,
    err_msg: *mut *const c_char,
) -> c_int {
    ffi_result(
        (|| {
            if json.is_null() {
                return Err("json is null".into());
            }
            let mode = MODE.lock().map_err(|error| error.to_string())?;
            let status = match &*mode {
                RunMode::Web(web) => web.status()?,
                _ => mode.instance()?.status()?,
            };
            let value =
                CString::new(serde_json::to_string(&status).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            unsafe {
                *json = value.into_raw();
            }
            Ok(())
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn set_instance_tun_fd(
    generation: u64,
    fd: c_int,
    err_msg: *mut *const c_char,
) -> c_int {
    ffi_result(
        (|| {
            let mode = MODE.lock().map_err(|error| error.to_string())?;
            mode.instance()?.set_tun_fd(generation, fd)
        })(),
        err_msg,
    )
}

#[no_mangle]
pub extern "C" fn stop_network_instance() -> c_int {
    let previous = match MODE.lock() {
        Ok(mut mode) => std::mem::replace(&mut *mode, RunMode::Idle),
        Err(_) => return -1,
    };
    let result = match previous {
        RunMode::Idle => return 0,
        RunMode::Local(instance) => instance.stop(),
        RunMode::Web(managed) => managed.stop(),
    };
    if result.is_ok() {
        0
    } else {
        -1
    }
}

#[no_mangle]
pub extern "C" fn get_running_info(json: *mut *const c_char, err_msg: *mut *const c_char) -> c_int {
    ffi_result(
        (|| {
            if json.is_null() {
                return Err("json is null".to_string());
            }
            let (manager, id) = active_instance()?;
            let infos = manager
                .collect_network_infos_sync()
                .map_err(|error| error.to_string())?;
            let info = infos
                .get(&id)
                .ok_or_else(|| "running info is unavailable".to_string())?;
            let value =
                CString::new(serde_json::to_string(info).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
            unsafe {
                *json = value.into_raw();
            }
            Ok(())
        })(),
        err_msg,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_full_url_and_short_token() {
        assert_eq!(
            normalize_config_server_endpoint("udp://127.0.0.1:22020/token").unwrap(),
            "udp://127.0.0.1:22020/token"
        );
        assert_eq!(
            normalize_config_server_endpoint("token").unwrap(),
            "udp://config-server.easytier.cn:22020/token"
        );
        assert_eq!(
            normalize_config_server_endpoint("https://example.com/token").unwrap(),
            "https://example.com/token"
        );
        assert_eq!(
            normalize_config_server_endpoint("token?#").unwrap(),
            "udp://config-server.easytier.cn:22020/token%3F%23"
        );
        assert!(normalize_config_server_endpoint("").is_err());
        assert!(normalize_config_server_endpoint("bad/token").is_err());
    }

    #[test]
    fn rejects_unsupported_scheme_and_missing_token() {
        for endpoint in [
            "ftp://example.com/token",
            "udp://127.0.0.1:22020",
            "udp://127.0.0.1:22020/",
        ] {
            assert!(
                normalize_config_server_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn local_ffi_uses_generation_checked_shared_lifecycle_across_restarts() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};
        static READY: AtomicU64 = AtomicU64::new(0);
        extern "C" fn on_event(json: *const c_char) {
            let event: serde_json::Value =
                serde_json::from_str(unsafe { CStr::from_ptr(json) }.to_str().unwrap()).unwrap();
            if event["event"] == "run" {
                READY.store(event["generation"].as_u64().unwrap(), Ordering::Release);
            }
        }
        let config = CString::new(
            "ipv4 = '10.42.0.1/24'\nlisteners = []\n[flags]\nno_tun = true\nenable_ipv6 = false\n",
        )
        .unwrap();
        let mut previous = 0;
        for _ in 0..2 {
            let mut error = std::ptr::null();
            assert_eq!(
                run_network_instance(config.as_ptr(), Some(on_event), &mut error),
                0
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while READY.load(Ordering::Acquire) <= previous {
                assert!(
                    Instant::now() < deadline,
                    "local FFI did not emit readiness"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let generation = READY.load(Ordering::Acquire);
            if previous != 0 {
                assert_eq!(
                    complete_instance_setup(previous, true, std::ptr::null()),
                    -1
                );
            }
            assert_eq!(
                complete_instance_setup(generation, true, std::ptr::null()),
                0
            );
            let mut json = std::ptr::null();
            assert_eq!(get_instance_status(&mut json, &mut error), 0);
            let status: serde_json::Value =
                serde_json::from_str(unsafe { CStr::from_ptr(json) }.to_str().unwrap()).unwrap();
            free_string(json);
            assert_eq!(status["status"], "running");
            assert_eq!(status["generation"], generation);
            assert_eq!(status["options"]["ipv4"], "10.42.0.1/24");
            assert_eq!(stop_network_instance(), 0);
            assert_eq!(
                complete_instance_setup(generation, true, std::ptr::null()),
                -1
            );
            previous = generation;
        }
    }

    #[test]
    fn status_serialization_uses_public_contract_keys() {
        let value = serde_json::to_value(InstanceStatus {
            status: "waiting_config",
            server_connected: true,
            instance_id: None,
            instance_name: None,
            network_name: None,
            generation: 3,
            error: None,
            options: None,
        })
        .unwrap();
        assert_eq!(value["status"], "waiting_config");
        assert_eq!(value["serverConnected"], true);
        assert_eq!(value["generation"], 3);
    }
}
