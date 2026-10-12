//! Win32 side of the Keel index service (`docs/index-service.md`): the `KeelIndex`
//! service in the Service Control Manager (install, uninstall, start, status), the
//! service's own entry point, and its named pipe with a per-user DACL.
//!
//! Security, in short (the doc has the full review):
//! - The pipe's DACL grants SYSTEM and exactly one user: the interactive user who
//!   installed the service (the SID is fixed at install time, in the service's
//!   command line, which only administrators can change). That user may read and
//!   write the pipe but not create instances of it (`0x12019b`: no
//!   `FILE_CREATE_PIPE_INSTANCE`). Remote clients are refused, and the first instance
//!   is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that grabbed the
//!   name first makes the service fail to start instead of sharing the name.
//! - The client opens the pipe with `SECURITY_IDENTIFICATION` (a server can never
//!   impersonate it) and checks that the process serving it is the one the Service
//!   Control Manager started as `KeelIndex`.
//! - The service runs as LocalSystem with only the privileges it needs, from an
//!   executable copied to `Program Files` (writable by administrators only), started
//!   on demand by its user and stopped after [`IDLE`] without a client.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    LocalFree, ERROR_ACCESS_DENIED, ERROR_CALL_NOT_IMPLEMENTED, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_SPECIFIC_ERROR,
    HANDLE, HLOCAL, NO_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    LookupAccountSidW, SidTypeUser, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SID_NAME_USE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
    RegisterServiceCtrlHandlerExW, SetServiceObjectSecurity, SetServiceStatus,
    StartServiceCtrlDispatcherW, StartServiceW, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_CREATE_SERVICE, SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL,
    SERVICE_QUERY_STATUS, SERVICE_REQUIRED_PRIVILEGES_INFOW, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

use super::service::{self, Client, Elevated, LocalVolumes};
use super::win::{is_elevated, run_elevated};

/// The service's name in the Service Control Manager.
pub(crate) const SERVICE_NAME: &str = "KeelIndex";
const DISPLAY_NAME: &str = "Keel Index Service";
const DESCRIPTION: &str = "Reads the NTFS file tables and change journals of this PC's fixed \
     drives for Keel's file index, for the one user who installed it. Started by Keel on \
     demand; stops when Keel is closed.";
/// `\\.\pipe\<PIPE_NAME>`.
pub(crate) const PIPE_NAME: &str = "KeelIndexService";
/// The service's copy of Keel, in `<Program Files>\Keel\IndexService`.
const EXE_NAME: &str = "keel-index-service.exe";
/// Everything the service needs; LocalSystem's other privileges are removed from its
/// token by the Service Control Manager.
const PRIVILEGES: [&str; 3] = [
    "SeChangeNotifyPrivilege",
    "SeBackupPrivilege",
    "SeManageVolumePrivilege",
];
/// The service stops after this long without a client.
const IDLE: Duration = Duration::from_secs(600);
/// Most clients served at once (Keel holds one connection per process).
const MAX_CLIENTS: usize = 4;
/// What the client asks of the pipe: read and write its data (no
/// `FILE_APPEND_DATA`, which on a pipe means creating an instance of it).
/// FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE.
const PIPE_CLIENT_ACCESS: u32 = 0x0010_0083;
/// What the pipe grants its user: FILE_GENERIC_READ | FILE_GENERIC_WRITE without
/// FILE_APPEND_DATA (0x4, which on a pipe is FILE_CREATE_PIPE_INSTANCE).
const PIPE_USER_RIGHTS: u32 = 0x0012_019b;
const _: () = assert!(
    PIPE_USER_RIGHTS & 0x4 == 0 && PIPE_USER_RIGHTS & PIPE_CLIENT_ACCESS == PIPE_CLIENT_ACCESS
);
/// WRITE_DAC | READ_CONTROL, standard rights.
const WRITE_DAC: u32 = 0x0004_0000;
const READ_CONTROL: u32 = 0x0002_0000;
const DELETE: u32 = 0x0001_0000;

fn win32(e: windows::core::Error) -> io::Error {
    let code = e.code().0 as u32;
    match code >> 16 {
        0x8007 => io::Error::from_raw_os_error((code & 0xFFFF) as i32),
        _ => io::Error::other(e),
    }
}

fn is_code(e: &io::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    e.raw_os_error() == Some(code.0 as i32)
}

// --- who may use the service ---

/// A SID string the service accepts as its user: a local or domain account
/// (`S-1-5-21-a-b-c-rid`) or a Microsoft Entra ID account (`S-1-12-1-a-b-c-d`), in
/// canonical form. Groups and well-known SIDs (Everyone, Users, ...) never match, and
/// nothing but digits and dashes gets into the SDDL or the command line built from it.
pub(crate) fn valid_user_sid(sid: &str) -> bool {
    let parts: Vec<&str> = sid.split('-').collect();
    let numbers = |p: &[&str]| {
        p.iter().all(|x| {
            (1..=10).contains(&x.len())
                && x.bytes().all(|b| b.is_ascii_digit())
                && !(x.len() > 1 && x.starts_with('0'))
                && x.parse::<u32>().is_ok()
        })
    };
    match parts.as_slice() {
        ["S", "1", "5", "21", rest @ ..] | ["S", "1", "12", "1", rest @ ..] => {
            rest.len() == 4 && numbers(rest)
        }
        _ => false,
    }
}

/// Whether `sid` names a user account (not a group or alias of the same form).
pub(crate) fn account_is_user(sid: &str) -> io::Result<bool> {
    let mut psid = PSID::default();
    // SAFETY: the string outlives the call; `psid` is freed below.
    unsafe { ConvertStringSidToSidW(&HSTRING::from(sid), &mut psid) }.map_err(win32)?;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE::default();
    // SAFETY: the buffers and lengths match; `psid` is a valid SID.
    let looked_up = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            psid,
            PWSTR(name.as_mut_ptr()),
            &mut name_len,
            PWSTR(domain.as_mut_ptr()),
            &mut domain_len,
            &mut kind,
        )
    };
    // SAFETY: allocated by ConvertStringSidToSidW.
    unsafe {
        let _ = LocalFree(HLOCAL(psid.0));
    }
    looked_up.map_err(win32)?;
    Ok(kind == SidTypeUser)
}

/// The pipe's security descriptor: SYSTEM full control, `sid` read and write (not
/// create-instance), nothing for anyone else; inherited entries cut off (`P`); a
/// medium integrity label so low-integrity (sandboxed) processes of the same user
/// cannot write to it.
pub(crate) fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;{PIPE_USER_RIGHTS:#x};;;{sid})S:(ML;;NW;;;ME)")
}

/// The service object's DACL: Windows' default for a service, plus start, stop and
/// query for `sid` (so its Keel starts the service on demand without elevation).
pub(crate) fn service_sddl(sid: &str) -> String {
    format!(
        "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)\
         (A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;CCLCSWRPWPLOCRRC;;;{sid})"
    )
}

/// The service's command line: the quoted executable (no unquoted-path lookup of
/// `C:\Program.exe`), then the user it serves.
pub(crate) fn command_line(exe: &Path, sid: &str) -> String {
    format!("\"{}\" --run-service --service-user {sid}", exe.display())
}

// --- security descriptors ---

/// A security descriptor built from SDDL (freed on drop).
struct Sd(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable once built and owned by this value.
unsafe impl Send for Sd {}

impl Sd {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the string outlives the call; `sd` is freed on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(win32)?;
        Ok(Self(sd))
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(HLOCAL(self.0 .0));
        }
    }
}

// --- the pipe ---

fn pipe_path(name: &str) -> HSTRING {
    HSTRING::from(format!(r"\\.\pipe\{name}"))
}

/// The server end of the service's pipe.
pub(crate) struct PipeServer {
    path: HSTRING,
    sd: Sd,
    /// The instance made by `bind`, before the first `accept`.
    pending: Option<OwnedHandle>,
}

impl PipeServer {
    /// Creates the first instance of `\\.\pipe\<name>` with `sddl`. Fails when the
    /// name is already taken (by anyone, this process included).
    pub fn bind(name: &str, sddl: &str) -> io::Result<Self> {
        let mut server = Self {
            path: pipe_path(name),
            sd: Sd::from_sddl(sddl)?,
            pending: None,
        };
        server.pending = Some(server.instance(true)?);
        Ok(server)
    }

    fn instance(&self, first: bool) -> io::Result<OwnedHandle> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0 .0,
            bInheritHandle: false.into(),
        };
        let mode = if first {
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_DUPLEX
        };
        // SAFETY: the name and attributes outlive the call.
        let handle = unsafe {
            CreateNamedPipeW(
                &self.path,
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                (MAX_CLIENTS + 1) as u32,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle this value now owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    /// Waits for the next client and returns its connection.
    pub fn accept(&mut self) -> io::Result<File> {
        let pipe = match self.pending.take() {
            Some(p) => p,
            None => self.instance(false)?,
        };
        // SAFETY: a live pipe handle; no overlapped I/O.
        match unsafe { ConnectNamedPipe(HANDLE(pipe.as_raw_handle()), None) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(e) => return Err(win32(e)),
        }
        // Make the next instance now, so the name never disappears between clients
        // (a client then waits for a free instance instead of finding no pipe). When
        // every instance is in use this fails and the next `accept` tries again.
        self.pending = self.instance(false).ok();
        Ok(File::from(pipe))
    }
}

/// Opens the client end of `\\.\pipe\<name>`, waiting up to `timeout` while every
/// instance is busy. The server may only identify this client, never impersonate it,
/// and `server_ok` must accept the serving process id (PermissionDenied otherwise).
pub(crate) fn connect_pipe(
    name: &str,
    timeout: Duration,
    server_ok: impl Fn(u32) -> bool,
) -> io::Result<File> {
    let path = pipe_path(name);
    let deadline = Instant::now() + timeout;
    let pipe = loop {
        // SAFETY: the name outlives the call; the handle is owned below.
        let opened = unsafe {
            CreateFileW(
                &path,
                PIPE_CLIENT_ACCESS,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                HANDLE::default(),
            )
        };
        match opened {
            // SAFETY: a fresh handle this value now owns.
            Ok(h) => break unsafe { OwnedHandle::from_raw_handle(h.0) },
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                let ms = left.as_millis().clamp(1, u32::MAX as u128) as u32;
                // SAFETY: the name outlives the call.
                let _ = unsafe { WaitNamedPipeW(&path, ms) };
            }
            Err(e) => return Err(win32(e)),
        }
    };
    let mut server = 0u32;
    // SAFETY: a live pipe handle and an out value.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server) }
        .map_err(win32)?;
    if !server_ok(server) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("pipe {name} is served by an unexpected process {server}"),
        ));
    }
    Ok(File::from(pipe))
}

/// Stop flag and activity of a running pipe server.
pub(crate) struct Control {
    name: String,
    stop: AtomicBool,
    active: AtomicUsize,
    last: Mutex<Instant>,
}

impl Control {
    pub fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        })
    }

    /// Makes [`serve_pipe`] return: sets the flag and connects once to wake the
    /// pending accept.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = connect_pipe(&self.name, Duration::from_millis(500), |_| true);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// No client for `idle`.
    pub fn idle_for(&self, idle: Duration) -> bool {
        self.active.load(Ordering::Acquire) == 0 && self.last.lock().elapsed() >= idle
    }
}

/// Accepts clients until [`Control::stop`], serving each on its own thread (at most
/// [`MAX_CLIENTS`] at once; more are disconnected).
pub(crate) fn serve_pipe(
    mut server: PipeServer,
    backend: Arc<dyn Elevated>,
    control: Arc<Control>,
) -> io::Result<()> {
    loop {
        let accepted = server.accept();
        if control.stopping() {
            return Ok(());
        }
        let mut conn = match accepted {
            Ok(c) => c,
            Err(e) => {
                super::warn(&format!("index service: accepting a client: {e}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if control.active.load(Ordering::Acquire) >= MAX_CLIENTS {
            continue;
        }
        control.active.fetch_add(1, Ordering::AcqRel);
        *control.last.lock() = Instant::now();
        let backend = backend.clone();
        let done = control.clone();
        let spawned = std::thread::Builder::new()
            .name("keel-index-client".into())
            .spawn(move || {
                let _ = service::serve(&mut conn, &*backend);
                // Let the client read the last answer before the pipe closes.
                // SAFETY: a live pipe handle.
                let _ = unsafe { FlushFileBuffers(HANDLE(conn.as_raw_handle())) };
                drop(conn);
                *done.last.lock() = Instant::now();
                done.active.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            // The connection went with the closure; nothing is being served.
            control.active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

// --- the Service Control Manager ---

/// The index service as the Service Control Manager reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Starting,
    Running,
    Stopping,
    /// The Service Control Manager could not be asked.
    Unknown,
}

/// What [`install_with`] registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceSpec {
    pub command: String,
    pub sddl: String,
}

/// The Service Control Manager calls the service code makes: [`WinScm`], or a mock.
pub(crate) trait Scm {
    /// State and process id (0 when not running).
    fn state(&self) -> io::Result<(ServiceState, u32)>;
    /// Creates the service, or updates its command line when it exists; then its
    /// description, required privileges and DACL.
    fn create_or_update(&self, spec: &ServiceSpec) -> io::Result<()>;
    fn start(&self) -> io::Result<()>;
    fn stop(&self) -> io::Result<()>;
    fn delete(&self) -> io::Result<()>;
}

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: opened by OpenSCManagerW / OpenServiceW / CreateServiceW.
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

/// The real Service Control Manager.
pub(crate) struct WinScm;

impl WinScm {
    fn manager(access: u32) -> io::Result<ScHandle> {
        // SAFETY: null names: the local machine's active database.
        unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access) }
            .map(ScHandle)
            .map_err(win32)
    }

    fn open(access: u32) -> io::Result<ScHandle> {
        let manager = Self::manager(SC_MANAGER_CONNECT)?;
        // SAFETY: a live manager handle; the name outlives the call.
        unsafe { OpenServiceW(manager.0, &HSTRING::from(SERVICE_NAME), access) }
            .map(ScHandle)
            .map_err(win32)
    }
}

/// A NUL-terminated (or, with `multi`, double-NUL-terminated) wide string list.
fn wide_list(items: &[&str]) -> Vec<u16> {
    items
        .iter()
        .flat_map(|s| s.encode_utf16().chain([0]))
        .chain([0])
        .collect()
}

impl Scm for WinScm {
    fn state(&self) -> io::Result<(ServiceState, u32)> {
        let service = match Self::open(SERVICE_QUERY_STATUS) {
            Ok(s) => s,
            Err(e) if is_code(&e, ERROR_SERVICE_DOES_NOT_EXIST) => {
                return Ok((ServiceState::NotInstalled, 0))
            }
            Err(e) => return Err(e),
        };
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0u32;
        // SAFETY: the byte view covers exactly `status`, which outlives the call.
        unsafe {
            let bytes = std::slice::from_raw_parts_mut(
                &mut status as *mut SERVICE_STATUS_PROCESS as *mut u8,
                size_of::<SERVICE_STATUS_PROCESS>(),
            );
            QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(bytes), &mut needed)
        }
        .map_err(win32)?;
        let state = status.dwCurrentState;
        let state = if state == SERVICE_STOPPED {
            ServiceState::Stopped
        } else if state == SERVICE_START_PENDING {
            ServiceState::Starting
        } else if state == SERVICE_RUNNING {
            ServiceState::Running
        } else if state == SERVICE_STOP_PENDING {
            ServiceState::Stopping
        } else {
            ServiceState::Unknown
        };
        Ok((state, status.dwProcessId))
    }

    fn create_or_update(&self, spec: &ServiceSpec) -> io::Result<()> {
        let manager = Self::manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
        let name = HSTRING::from(SERVICE_NAME);
        let display = HSTRING::from(DISPLAY_NAME);
        let command = HSTRING::from(spec.command.as_str());
        // SAFETY: live handles; every string outlives its call.
        let service = match unsafe {
            OpenServiceW(
                manager.0,
                &name,
                SERVICE_CHANGE_CONFIG | WRITE_DAC | READ_CONTROL,
            )
        } {
            Ok(h) => {
                let h = ScHandle(h);
                // SAFETY: as above; null strings leave those settings as they are.
                unsafe {
                    ChangeServiceConfigW(
                        h.0,
                        SERVICE_WIN32_OWN_PROCESS,
                        SERVICE_DEMAND_START,
                        SERVICE_ERROR_NORMAL,
                        &command,
                        PCWSTR::null(),
                        None,
                        PCWSTR::null(),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        &display,
                    )
                }
                .map_err(win32)?;
                h
            }
            Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
                // SAFETY: as above; a null account name is LocalSystem.
                ScHandle(
                    unsafe {
                        CreateServiceW(
                            manager.0,
                            &name,
                            &display,
                            SERVICE_ALL_ACCESS,
                            SERVICE_WIN32_OWN_PROCESS,
                            SERVICE_DEMAND_START,
                            SERVICE_ERROR_NORMAL,
                            &command,
                            PCWSTR::null(),
                            None,
                            PCWSTR::null(),
                            PCWSTR::null(),
                            PCWSTR::null(),
                        )
                    }
                    .map_err(win32)?,
                )
            }
            Err(e) => return Err(win32(e)),
        };
        let mut description = wide_list(&[DESCRIPTION]);
        let info = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(description.as_mut_ptr()),
        };
        // SAFETY: `info` and its string outlive the call.
        unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&info as *const SERVICE_DESCRIPTIONW as *const c_void),
            )
        }
        .map_err(win32)?;
        let mut privileges = wide_list(&PRIVILEGES);
        let info = SERVICE_REQUIRED_PRIVILEGES_INFOW {
            pmszRequiredPrivileges: PWSTR(privileges.as_mut_ptr()),
        };
        // SAFETY: `info` and its multi-string outlive the call.
        unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
                Some(&info as *const SERVICE_REQUIRED_PRIVILEGES_INFOW as *const c_void),
            )
        }
        .map_err(win32)?;
        let sd = Sd::from_sddl(&spec.sddl)?;
        // SAFETY: a live service handle opened with WRITE_DAC and a valid descriptor.
        unsafe { SetServiceObjectSecurity(service.0, DACL_SECURITY_INFORMATION, sd.0) }
            .map_err(win32)
    }

    fn start(&self) -> io::Result<()> {
        let service = Self::open(SERVICE_START)?;
        // SAFETY: a live service handle; no arguments.
        match unsafe { StartServiceW(service.0, None) }.map_err(win32) {
            Err(e) if is_code(&e, ERROR_SERVICE_ALREADY_RUNNING) => Ok(()),
            other => other,
        }
    }

    fn stop(&self) -> io::Result<()> {
        let service = Self::open(SERVICE_STOP)?;
        let mut status = SERVICE_STATUS::default();
        // SAFETY: a live service handle and an out struct.
        match unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) }.map_err(win32)
        {
            Err(e) if is_code(&e, ERROR_SERVICE_NOT_ACTIVE) => Ok(()),
            other => other,
        }
    }

    fn delete(&self) -> io::Result<()> {
        let service = match Self::open(DELETE) {
            Err(e) if is_code(&e, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
            other => other?,
        };
        // SAFETY: a live service handle opened with DELETE.
        match unsafe { DeleteService(service.0) }.map_err(win32) {
            Err(e) if is_code(&e, ERROR_SERVICE_MARKED_FOR_DELETE) => Ok(()),
            other => other,
        }
    }
}

/// Polls `scm` until `done` holds for its state, or `timeout`.
fn wait_for(
    scm: &dyn Scm,
    done: impl Fn(ServiceState) -> bool,
    timeout: Duration,
) -> io::Result<(ServiceState, u32)> {
    let deadline = Instant::now() + timeout;
    loop {
        let (state, pid) = scm.state()?;
        if done(state) {
            return Ok((state, pid));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the index service is still {state:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop_and_wait(scm: &dyn Scm) -> io::Result<()> {
    let (state, _) = scm.state()?;
    if matches!(state, ServiceState::NotInstalled | ServiceState::Stopped) {
        return Ok(());
    }
    scm.stop()?;
    wait_for(
        scm,
        |s| matches!(s, ServiceState::Stopped | ServiceState::NotInstalled),
        Duration::from_secs(20),
    )
    .map(|_| ())
}

/// Installs (or updates) the service for `sid`: stops a running one, copies `exe`
/// into `dir` (a folder only administrators can write: the service runs that copy as
/// LocalSystem), then registers it. Returns the copy's path.
pub(crate) fn install_with(
    scm: &dyn Scm,
    sid: &str,
    exe: &Path,
    dir: &Path,
) -> io::Result<PathBuf> {
    if !valid_user_sid(sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{sid:?} is not a user account SID"),
        ));
    }
    stop_and_wait(scm)?;
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(EXE_NAME);
    let tmp = dir.join(format!("{EXE_NAME}.new"));
    std::fs::copy(exe, &tmp)?;
    std::fs::rename(&tmp, &dest)?;
    scm.create_or_update(&ServiceSpec {
        command: command_line(&dest, sid),
        sddl: service_sddl(sid),
    })?;
    Ok(dest)
}

/// Stops and deletes the service and removes its copy of Keel from `dir` (and `dir`,
/// and its parent, when they are left empty). False when it was not installed.
pub(crate) fn uninstall_with(scm: &dyn Scm, dir: &Path) -> io::Result<bool> {
    let (state, _) = scm.state()?;
    let installed = state != ServiceState::NotInstalled;
    if installed {
        stop_and_wait(scm)?;
        scm.delete()?;
    }
    // The executable is released when the stopped service's process exits.
    for _ in 0..40 {
        match std::fs::remove_file(dir.join(EXE_NAME)) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    let _ = std::fs::remove_file(dir.join(format!("{EXE_NAME}.new")));
    if std::fs::remove_dir(dir).is_ok() {
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(installed)
}

/// `<Program Files>\Keel\IndexService`, from the known-folder registry (not the
/// environment, which the user controls).
fn install_dir() -> io::Result<PathBuf> {
    // SAFETY: a known folder id; the returned string is freed below.
    let path =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, HANDLE::default()) }
            .map_err(win32)?;
    // SAFETY: a NUL-terminated string from the shell.
    let text = unsafe { path.to_string() };
    // SAFETY: allocated by SHGetKnownFolderPath.
    unsafe { CoTaskMemFree(Some(path.0 as *const c_void)) };
    let text = text.map_err(io::Error::other)?;
    Ok(PathBuf::from(text).join("Keel").join("IndexService"))
}

/// The service's state, `Unknown` when the Service Control Manager cannot be asked.
pub fn service_state() -> ServiceState {
    WinScm
        .state()
        .map_or(ServiceState::Unknown, |(state, _)| state)
}

/// Body of `keel --install-service` (administrator): installs the service for
/// `user_sid`, or for the account this process runs as. Returns the path of the
/// service's executable.
pub fn install_service(user_sid: Option<&str>) -> anyhow::Result<PathBuf> {
    use anyhow::{bail, Context};
    if !is_elevated() {
        bail!("installing the index service needs administrator rights");
    }
    let sid = match user_sid {
        Some(s) => s.to_owned(),
        None => keel_vfs::pipe::user_sid()?,
    };
    if !valid_user_sid(&sid) {
        bail!("{sid:?} is not a user account SID");
    }
    if !account_is_user(&sid).with_context(|| format!("looking up {sid}"))? {
        bail!("{sid} is not a user account");
    }
    let exe = std::env::current_exe()?;
    let dir = install_dir()?;
    install_with(&WinScm, &sid, &exe, &dir).context("installing the index service")
}

/// Body of `keel --uninstall-service` (administrator). False when it was not
/// installed.
pub fn uninstall_service() -> anyhow::Result<bool> {
    use anyhow::{bail, Context};
    if !is_elevated() {
        bail!("removing the index service needs administrator rights");
    }
    uninstall_with(&WinScm, &install_dir()?).context("removing the index service")
}

/// Installs (for this user) or removes the service: directly when this process is
/// elevated, else by running `keel --install-service --service-user <this user>` or
/// `keel --uninstall-service` through the UAC prompt and waiting for it. Blocks: call
/// off the UI thread.
pub fn request_service_change(install: bool) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    if is_elevated() {
        if install {
            install_service(None)?;
        } else {
            uninstall_service()?;
        }
        return Ok(());
    }
    let args = if install {
        format!(
            "--install-service --service-user {}",
            keel_vfs::pipe::user_sid()?
        )
    } else {
        "--uninstall-service".to_owned()
    };
    let exe = std::env::current_exe()?;
    let code = run_elevated(&exe, &args).context("asking Windows for administrator rights")?;
    if code != 0 {
        bail!("the elevated Keel exited with code {code}");
    }
    Ok(())
}

/// Connects to the installed service, starting it when it is stopped (its DACL lets
/// the installing user start it), and checks that the pipe is served by the
/// service's own process. NotFound when it is not installed.
pub(crate) fn connect_service() -> io::Result<Client<File>> {
    let scm = WinScm;
    let (state, _) = scm.state()?;
    match state {
        ServiceState::NotInstalled => return Err(io::ErrorKind::NotFound.into()),
        ServiceState::Stopped => scm.start()?,
        _ => {}
    }
    let (state, pid) = wait_for(
        &scm,
        |s| !matches!(s, ServiceState::Starting | ServiceState::Stopped),
        Duration::from_secs(15),
    )?;
    if state != ServiceState::Running || pid == 0 {
        return Err(io::Error::other(format!("the index service is {state:?}")));
    }
    let pipe = connect_pipe(PIPE_NAME, Duration::from_secs(5), |server| server == pid)?;
    let client = Client::handshake(pipe)?;
    if client.pid != pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the index service named another process",
        ));
    }
    Ok(client)
}

// --- the service process ---

static SERVICE_USER: OnceLock<String> = OnceLock::new();
static CONTROL: OnceLock<Arc<Control>> = OnceLock::new();
static STATUS: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicBool = AtomicBool::new(false);

/// Body of `keel --run-service --service-user <SID>`, which only the Service Control
/// Manager starts: serves [`PIPE_NAME`] for that user until stopped. Errors at once
/// when run from a console.
pub fn run_service(user_sid: &str) -> anyhow::Result<()> {
    if !valid_user_sid(user_sid) {
        anyhow::bail!("{user_sid:?} is not a user account SID");
    }
    let _ = SERVICE_USER.set(user_sid.to_owned());
    let mut name: Vec<u16> = wide_list(&[SERVICE_NAME]);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table and its name outlive the dispatcher, which returns once the
    // service has stopped.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(win32)?;
    if FAILED.load(Ordering::Acquire) {
        anyhow::bail!("the index service failed");
    }
    Ok(())
}

fn report(state: SERVICE_STATUS_CURRENT_STATE, failed: bool) {
    let handle = SERVICE_STATUS_HANDLE(STATUS.load(Ordering::Acquire) as *mut c_void);
    if handle.0.is_null() {
        return;
    }
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if failed {
            ERROR_SERVICE_SPECIFIC_ERROR.0
        } else {
            NO_ERROR.0
        },
        dwServiceSpecificExitCode: u32::from(failed),
        dwCheckPoint: 0,
        dwWaitHint: if pending { 10_000 } else { 0 },
    };
    // SAFETY: a handle from RegisterServiceCtrlHandlerExW and a live struct.
    let _ = unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    // SAFETY: the name outlives the call; the handler is a plain function.
    let registered = unsafe {
        RegisterServiceCtrlHandlerExW(&HSTRING::from(SERVICE_NAME), Some(control_handler), None)
    };
    let Ok(handle) = registered else {
        FAILED.store(true, Ordering::Release);
        return;
    };
    STATUS.store(handle.0 as usize, Ordering::Release);
    report(SERVICE_START_PENDING, false);
    let result = serve_service();
    if let Err(e) = &result {
        super::warn(&format!("index service: {e:#}"));
        FAILED.store(true, Ordering::Release);
    }
    report(SERVICE_STOPPED, result.is_err());
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, false);
            if let Some(control) = CONTROL.get() {
                control.stop();
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

/// Binds the pipe (failing when its name is taken), reports running, and serves until
/// stopped or idle for [`IDLE`].
fn serve_service() -> anyhow::Result<()> {
    let sid = SERVICE_USER
        .get()
        .ok_or_else(|| anyhow::anyhow!("no service user"))?;
    let control = CONTROL.get_or_init(|| Control::new(PIPE_NAME)).clone();
    let server = PipeServer::bind(PIPE_NAME, &pipe_sddl(sid)).map_err(|e| {
        if is_code(&e, ERROR_ACCESS_DENIED) {
            anyhow::anyhow!("another process holds the pipe name {PIPE_NAME}")
        } else {
            anyhow::Error::from(e)
        }
    })?;
    report(SERVICE_RUNNING, false);
    let idle = control.clone();
    let _ = std::thread::Builder::new()
        .name("keel-index-idle".into())
        .spawn(move || {
            while !idle.stopping() {
                std::thread::sleep(Duration::from_secs(5));
                if idle.idle_for(IDLE) {
                    idle.stop();
                }
            }
        });
    serve_pipe(server, Arc::new(LocalVolumes::default()), control)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::service::tests::Mock;
    use crate::proto::tests::sample_row;
    use std::cell::RefCell;

    #[test]
    fn user_sids_are_checked_before_they_reach_sddl() {
        for good in [
            "S-1-5-21-1004336348-1177238915-682003330-1001",
            "S-1-5-21-1-2-3-500",
            "S-1-12-1-1111111111-2222222222-3333333333-4044444444",
        ] {
            assert!(valid_user_sid(good), "{good}");
        }
        for bad in [
            "",
            "S-1-1-0",      // Everyone
            "S-1-5-32-545", // Users
            "S-1-5-11",     // Authenticated Users
            "S-1-5-18",     // SYSTEM
            "S-1-5-21-1-2-3",
            "S-1-5-21-1-2-3-4-5",
            "S-1-5-21-1-2-3-01",
            "S-1-5-21-1-2-3-99999999999",
            "S-1-5-21-1-2-3-1001)(A;;GA;;;WD",
            "S-1-5-21-1-2-3-1001 --other",
            "s-1-5-21-1-2-3-1001",
            "S-1-5-21-1-2-3-+1",
            "WD",
        ] {
            assert!(!valid_user_sid(bad), "{bad}");
        }
    }

    #[test]
    fn this_user_is_a_user_account() {
        let me = keel_vfs::pipe::user_sid().unwrap();
        assert!(valid_user_sid(&me), "{me}");
        assert!(account_is_user(&me).unwrap());
        // A well-formed SID that names no account.
        assert!(!matches!(account_is_user("S-1-5-21-1-2-3-4"), Ok(true)));
    }

    #[test]
    fn descriptors_grant_only_system_and_the_user() {
        let sid = "S-1-5-21-1-2-3-1001";
        let pipe = pipe_sddl(sid);
        assert_eq!(
            pipe,
            "D:P(A;;GA;;;SY)(A;;0x12019b;;;S-1-5-21-1-2-3-1001)S:(ML;;NW;;;ME)"
        );
        for group in ["WD", "AU", "BU", "IU", "AN", "BA"] {
            assert!(!pipe.contains(&format!(";;;{group})")), "{group}");
        }
        let service = service_sddl(sid);
        assert!(service.ends_with("(A;;CCLCSWRPWPLOCRRC;;;S-1-5-21-1-2-3-1001)"));
        // Only SYSTEM and administrators may reconfigure or delete it.
        for entry in service
            .split(')')
            .filter(|e| e.contains("DC") || e.contains("SD"))
        {
            assert!(entry.ends_with(";;;BA"), "{entry}");
        }
        assert!(Sd::from_sddl(&pipe).is_ok());
        assert!(Sd::from_sddl(&service).is_ok());
        assert_eq!(
            command_line(Path::new(r"C:\Program Files\Keel\IndexService\k.exe"), sid),
            r#""C:\Program Files\Keel\IndexService\k.exe" --run-service --service-user S-1-5-21-1-2-3-1001"#
        );
    }

    /// The protocol over a real pipe in this process, with the service's own DACL for
    /// this user.
    #[test]
    fn serves_this_user_over_a_real_pipe() {
        let name = format!("keel-index-test-{}", std::process::id());
        let me = keel_vfs::pipe::user_sid().unwrap();
        let server = PipeServer::bind(&name, &pipe_sddl(&me)).unwrap();
        // The name is taken now: a second server (a squatter) cannot share it.
        assert!(PipeServer::bind(&name, &pipe_sddl(&me)).is_err());
        let backend: Arc<dyn Elevated> = Arc::new(Mock {
            rows: vec![sample_row(0x0001_0000_0000_0020, 5, "a.txt", false)],
            ..Mock::default()
        });
        let control = Control::new(&name);
        let thread = {
            let control = control.clone();
            std::thread::spawn(move || serve_pipe(server, backend, control))
        };
        let pid = std::process::id();
        // A client that expects another server process refuses this one.
        let err = connect_pipe(&name, Duration::from_secs(5), |p| p != pid).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let pipe = connect_pipe(&name, Duration::from_secs(5), |p| p == pid).unwrap();
        let mut client = Client::handshake(pipe).unwrap();
        assert_eq!(client.pid, pid);
        let rows = RefCell::new(Vec::new());
        assert!(client
            .read_mft(
                'C',
                |r| rows.borrow_mut().push(r),
                || rows.borrow_mut().clear()
            )
            .unwrap());
        assert_eq!(rows.into_inner().len(), 1);
        assert_eq!(client.query_journal('C').unwrap().id, 42);
        // Two clients at once.
        let pipe = connect_pipe(&name, Duration::from_secs(5), |p| p == pid).unwrap();
        let second = Client::handshake(pipe).unwrap();
        assert_eq!(second.pid, pid);
        drop((client, second));
        control.stop();
        thread.join().unwrap().unwrap();
    }

    #[test]
    fn the_pipe_refuses_other_users() {
        let name = format!("keel-index-test-other-{}", std::process::id());
        // Granted to SYSTEM and another (made-up) account only: this process, even as
        // the pipe's owner, may not read or write it.
        let _server = PipeServer::bind(&name, &pipe_sddl("S-1-5-21-1-2-3-1001")).unwrap();
        let err = connect_pipe(&name, Duration::from_secs(2), |_| true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    /// A Service Control Manager in memory.
    #[derive(Default)]
    struct MockScm {
        state: Mutex<Option<(ServiceState, u32)>>,
        calls: Mutex<Vec<String>>,
        spec: Mutex<Option<ServiceSpec>>,
    }

    impl Scm for MockScm {
        fn state(&self) -> io::Result<(ServiceState, u32)> {
            Ok(self.state.lock().unwrap_or((ServiceState::NotInstalled, 0)))
        }
        fn create_or_update(&self, spec: &ServiceSpec) -> io::Result<()> {
            self.calls.lock().push("create".into());
            *self.spec.lock() = Some(spec.clone());
            let mut state = self.state.lock();
            if state.is_none() {
                *state = Some((ServiceState::Stopped, 0));
            }
            Ok(())
        }
        fn start(&self) -> io::Result<()> {
            self.calls.lock().push("start".into());
            *self.state.lock() = Some((ServiceState::Running, 77));
            Ok(())
        }
        fn stop(&self) -> io::Result<()> {
            self.calls.lock().push("stop".into());
            *self.state.lock() = Some((ServiceState::Stopped, 0));
            Ok(())
        }
        fn delete(&self) -> io::Result<()> {
            self.calls.lock().push("delete".into());
            *self.state.lock() = None;
            Ok(())
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-svc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_copies_keel_and_registers_the_service() {
        let base = temp("install");
        let exe = base.join("keel.exe");
        std::fs::write(&exe, b"keel").unwrap();
        let dir = base.join("Keel").join("IndexService");
        let scm = MockScm::default();
        let sid = "S-1-5-21-1-2-3-1001";
        let dest = install_with(&scm, sid, &exe, &dir).unwrap();
        assert_eq!(dest, dir.join(EXE_NAME));
        assert_eq!(std::fs::read(&dest).unwrap(), b"keel");
        assert_eq!(*scm.calls.lock(), ["create"]);
        let spec = scm.spec.lock().clone().unwrap();
        assert_eq!(spec.command, command_line(&dest, sid));
        assert_eq!(spec.sddl, service_sddl(sid));

        // Again while it runs: stopped first, the copy replaced, the config updated.
        scm.start().unwrap();
        std::fs::write(&exe, b"keel 2").unwrap();
        install_with(&scm, sid, &exe, &dir).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"keel 2");
        assert_eq!(*scm.calls.lock(), ["create", "start", "stop", "create"]);

        // A SID that would inject SDDL never gets as far as the SCM.
        let err = install_with(&scm, "S-1-5-21-1-2-3-1001)(A;;GA;;;WD", &exe, &dir).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(scm.calls.lock().len(), 4);

        // Uninstall: stopped, deleted, the copy and the empty folders removed.
        scm.start().unwrap();
        assert!(uninstall_with(&scm, &dir).unwrap());
        assert_eq!(
            *scm.calls.lock(),
            ["create", "start", "stop", "create", "start", "stop", "delete"]
        );
        assert!(!dir.exists());
        assert!(!base.join("Keel").exists());
        assert!(exe.exists(), "the user's own Keel stays");
        assert!(!uninstall_with(&scm, &dir).unwrap(), "already gone");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn service_state_answers_without_elevation() {
        // Installed or not, asking never needs administrator rights.
        assert_ne!(service_state(), ServiceState::Unknown);
    }
}
