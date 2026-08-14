use std::{ffi::CString, fs::File, io::{self, Seek, SeekFrom, Write}, sync::{Arc, Mutex}};

use easytier::{
    common::{
        config::{process_secure_mode_cfg, ConfigFileControl, ConfigLoader, TomlConfigLoader},
        global_ctx::GlobalCtxEvent,
    },
    launcher::NetworkInstance,
};
use once_cell::sync::Lazy;
use tracing_oslog::OsLogger;
use tracing_subscriber::layer::SubscriberExt as _;

static INSTANCE: Lazy<Arc<Mutex<Option<NetworkInstance>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));
type SharedLogFile = Arc<Mutex<File>>;
static LOGGER_FILE: Lazy<Arc<Mutex<Option<SharedLogFile>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));
static LOGGER_READY: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

fn prepare_network_config(cfg_str: &str) -> Result<TomlConfigLoader, String> {
    let cfg = TomlConfigLoader::new_from_str(cfg_str).map_err(|e| e.to_string())?;
    if let Some(secure_mode) = cfg.get_secure_mode() {
        let secure_mode = process_secure_mode_cfg(secure_mode).map_err(|e| e.to_string())?;
        if secure_mode.enabled {
            let private_key = secure_mode.private_key().map_err(|e| e.to_string())?;
            let public_key = secure_mode.public_key().map_err(|e| e.to_string())?;
            let derived_public_key = x25519_dalek::PublicKey::from(&private_key);
            if public_key.as_bytes() != derived_public_key.as_bytes() {
                return Err("local public key does not match local private key".to_string());
            }
        }
        cfg.set_secure_mode(Some(secure_mode));
    }
    Ok(cfg)
}

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
        let mut file = self.file.lock().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        maybe_rotate_log_file(&mut file, self.max_bytes, buf.len())?;
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self.file.lock().map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        file.flush()
    }
}

/// # Safety
/// Initialize logger.
/// `max_bytes`: 0 means use [`DEFAULT_MAX_LOG_BYTES`].
/// `enable_file_log`: non-zero writes `easytier.log`; zero keeps OSLog only.
#[no_mangle]
pub extern "C" fn init_logger(
    path: *const std::ffi::c_char,
    level: *const std::ffi::c_char,
    subsystem: *const std::ffi::c_char,
    max_bytes: u64,
    enable_file_log: std::ffi::c_int,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let path = unsafe {
        std::ffi::CStr::from_ptr(path)
            .to_string_lossy()
            .into_owned()
    };
    let level = unsafe {
        std::ffi::CStr::from_ptr(level)
            .to_string_lossy()
            .into_owned()
    };
    let subsystem = unsafe {
        std::ffi::CStr::from_ptr(subsystem)
            .to_string_lossy()
            .into_owned()
    };
    let max_bytes = if max_bytes == 0 {
        DEFAULT_MAX_LOG_BYTES
    } else {
        max_bytes
    };
    let enable_file_log = enable_file_log != 0;

    let impl_func = || {
        let mut ready = LOGGER_READY.lock().map_err(|e| e.to_string())?;
        if *ready {
            return Ok::<(), String>(());
        }

        let filter = tracing_subscriber::EnvFilter::new(level);
        if enable_file_log {
            let file = Arc::new(Mutex::new(File::create(path).map_err(|e| e.to_string())?));
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
                .with(OsLogger::new(&subsystem, "rust"));
            tracing::subscriber::set_global_default(collector).map_err(|e| e.to_string())?;
            *LOGGER_FILE.lock().map_err(|e| e.to_string())? = Some(file);
        } else {
            let collector = tracing_subscriber::registry()
                .with(filter)
                .with(OsLogger::new(&subsystem, "rust"));
            tracing::subscriber::set_global_default(collector).map_err(|e| e.to_string())?;
        }
        *ready = true;
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

#[no_mangle]
/// # Safety
/// Clear the currently initialized file logger and reset its file offset.
/// No-op (success) when file logging was never enabled.
pub extern "C" fn clear_logger(err_msg: *mut *const std::ffi::c_char) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        let file = match LOGGER_FILE
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
        {
            Some(file) => file,
            None => return Ok(()),
        };
        let mut file = file.lock().map_err(|e| e.to_string())?;
        file.set_len(0).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

/// # Safety
/// Set the tun fd
#[no_mangle]
pub extern "C" fn set_tun_fd(
    fd: std::ffi::c_int,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        let mut inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let inst = inst.as_mut().ok_or("no running instance".to_string())?;
        let sender = inst.get_tun_fd_sender().ok_or("tun fd sender is null".to_string())?;
        sender.try_send(Some(fd)).map_err(|e| e.to_string())?;
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

#[no_mangle]
/// # Safety
/// The pointer `s` must have been returned by one of this library's FFI
/// functions that allocate strings (for example, via `CString::into_raw()`),
/// and it must not have been freed previously. Passing any other pointer, or
/// a pointer that has already been freed, results in undefined behavior.
/// It is allowed to pass a null pointer; in that case this function is a no-op.
pub extern "C" fn free_string(s: *const std::ffi::c_char) {
    if s.is_null() { return; }
    unsafe {
        let _ = std::ffi::CString::from_raw(s as *mut std::ffi::c_char);
    }
}

/// # Safety
/// Run the network instance
#[no_mangle]
pub extern "C" fn run_network_instance(
    cfg_str: *const std::ffi::c_char,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || {
        if cfg_str.is_null() {
            return Err("cfg_str is nullptr".to_string());
        }
        let cfg_str = unsafe {
            std::ffi::CStr::from_ptr(cfg_str)
                .to_string_lossy()
                .into_owned()
        };
        let cfg = prepare_network_config(&cfg_str)?;
        let mut inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let mut new_inst = NetworkInstance::new(cfg, ConfigFileControl::STATIC_CONFIG);
        new_inst.start().map_err(|e| e.to_string())?;
        *inst = Some(new_inst);
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

/// # Safety
/// Stop the network instance
#[no_mangle]
pub extern "C" fn stop_network_instance() -> std::ffi::c_int {
    match INSTANCE.lock() {
        Ok(mut inst) => {
            inst.as_mut()
                .and_then(|inst| inst.get_stop_notifier())
                .map(|stop| stop.notify_waiters());
            *inst = None;
            0
        },
        Err(_) => -1,
    }
}

/// # Safety
/// Register stop callback
#[no_mangle]
pub extern "C" fn register_stop_callback(
    callback: Option<extern "C" fn()>,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        let callback = callback.ok_or("callback is null".to_string())?;
        let inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let inst = inst.as_ref().ok_or("no running instance".to_string())?;
        let stop = inst.get_stop_notifier().ok_or("no stop notifier".to_string())?;
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new();
            if let Ok(runtime) = runtime {
                runtime.block_on(stop.notified());
                callback();
            } else {
                tracing::error!("failed to create runtime for stop callback");
            }
        });
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

/// # Safety
/// Register running info callback
#[no_mangle]
pub extern "C" fn register_running_info_callback(
    callback: Option<extern "C" fn()>,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        let callback = callback.ok_or("callback is null".to_string())?;
        let inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let inst = inst.as_ref().ok_or("no running instance".to_string())?;
        let mut ev = inst
            .subscribe_event()
            .ok_or("no event subscriber".to_string())?;
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new();
            if let Ok(runtime) = runtime {
                runtime.block_on(async move {
                    loop {
                        match ev.recv().await {
                            Ok(event) => match event {
                                GlobalCtxEvent::DhcpIpv4Changed(_, _)
                                | GlobalCtxEvent::ProxyCidrsUpdated(_, _)
                                | GlobalCtxEvent::ConfigPatched(_) => {
                                    callback();
                                }
                                _ => {}
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue;
                            }
                        }
                    }
                });
            } else {
                tracing::error!("failed to create runtime for running info callback");
            }
        });
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

/// # Safety
/// Get running info
#[no_mangle]
pub extern "C" fn get_running_info(
    json: *mut *const std::ffi::c_char,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        if json.is_null() {
            return Err("json is a nullptr".to_string());
        }
        let inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let inst = inst.as_ref().ok_or("no running instance".to_string())?;
        let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let info = runtime.block_on(inst.get_running_info()).map_err(|e| e.to_string())?;
        let info = serde_json::to_string(&info).map_err(|e| e.to_string())?;
        let cstr = CString::new(info).map_err(|e| e.to_string())?;
        unsafe {
            *json = cstr.into_raw()
        }
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

/// # Safety
/// Get latest error message
#[no_mangle]
pub extern "C" fn get_latest_error_msg(
    msg: *mut *const std::ffi::c_char,
    err_msg: *mut *const std::ffi::c_char,
) -> std::ffi::c_int {
    let impl_func = || -> Result<(), String> {
        if msg.is_null() {
            return Err("msg is a nullptr".to_string());
        }
        let inst = INSTANCE.lock().map_err(|e| e.to_string())?;
        let inst = inst.as_ref().ok_or("no running instance".to_string())?;
        let latest = inst.get_latest_error_msg();
        if let Some(latest) = latest {
            let cstr = CString::new(latest).map_err(|e| e.to_string())?;
            unsafe { *msg = cstr.into_raw(); }
        } else {
            unsafe { *msg = std::ptr::null(); }
        }
        Ok(())
    };

    match impl_func() {
        Ok(_) => 0,
        Err(e) => {
            if !err_msg.is_null() {
                if let Ok(cstr) = CString::new(e) {
                    unsafe { *err_msg = cstr.into_raw(); }
                };
            }
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_network_config_generates_secure_mode_keys() {
        let cfg = prepare_network_config(
            r#"
                [network_identity]
                network_name = "secure-test"
                network_secret = ""

                [secure_mode]
                enabled = true
            "#,
        )
        .unwrap();

        let secure_mode = cfg.get_secure_mode().unwrap();
        assert!(secure_mode.local_private_key.is_some());
        assert!(secure_mode.local_public_key.is_some());
        assert!(secure_mode.private_key().is_ok());
        assert!(secure_mode.public_key().is_ok());
    }

    #[test]
    fn test_prepare_network_config_rejects_invalid_secure_mode_key() {
        let result = prepare_network_config(
            r#"
                [network_identity]
                network_name = "secure-test"
                network_secret = ""

                [secure_mode]
                enabled = true
                local_private_key = "invalid"
            "#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_prepare_network_config_rejects_mismatched_secure_mode_keypair() {
        let result = prepare_network_config(
            r#"
                [network_identity]
                network_name = "secure-test"
                network_secret = ""

                [secure_mode]
                enabled = true
                local_private_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                local_public_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            "#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_run_network_instance() {
        let cfg_str = r#"
            inst_name = "test"
            network = "test_network"
        "#;
        let cstr = std::ffi::CString::new(cfg_str).unwrap();
        assert_eq!(run_network_instance(cstr.as_ptr(), std::ptr::null_mut()), 0);
    }
}
