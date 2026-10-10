//! WinFsp backend (Windows, feature `winfsp`): a drive letter (`K:`) or a folder that does
//! not exist yet, served by [`MountFs`] in this process. Needs WinFsp installed.

use crate::fs::{Attr, MountFs};
use crate::path::MountPath;
use std::ffi::c_void;
use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};

const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// `FspCleanupDelete`.
const CLEANUP_DELETE: u32 = 0x01;

const STATUS_OBJECT_NAME_INVALID: u32 = 0xC000_0033;
const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_SHARING_VIOLATION: u32 = 0xC000_0043;
const STATUS_END_OF_FILE: u32 = 0xC000_0011;
const STATUS_DEVICE_NOT_CONNECTED: u32 = 0xC000_009D;
const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
const STATUS_DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
const STATUS_FILE_CLOSED: u32 = 0xC000_0128;
const STATUS_MEDIA_WRITE_PROTECTED: u32 = 0xC000_00A2;

fn status(code: u32) -> FspError {
    FspError::NTSTATUS(code as i32)
}

/// An NTSTATUS for an `io::Error` (OS errors keep their Win32 code).
fn nt(e: io::Error) -> FspError {
    use io::ErrorKind as K;
    if e.raw_os_error().is_some() {
        return e.into();
    }
    status(match e.kind() {
        K::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        K::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        K::PermissionDenied => STATUS_ACCESS_DENIED,
        K::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        K::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        K::NotADirectory => STATUS_NOT_A_DIRECTORY,
        K::InvalidInput => STATUS_OBJECT_NAME_INVALID,
        K::ResourceBusy => STATUS_SHARING_VIOLATION,
        K::NotConnected => STATUS_DEVICE_NOT_CONNECTED,
        K::BrokenPipe => STATUS_FILE_CLOSED,
        K::ReadOnlyFilesystem => STATUS_MEDIA_WRITE_PROTECTED,
        _ => return e.into(),
    })
}

fn path(name: &U16CStr) -> winfsp::Result<MountPath> {
    MountPath::parse(&name.to_string_lossy()).map_err(nt)
}

/// 100 ns ticks since 1601 (FILETIME).
fn filetime(t: Option<SystemTime>) -> u64 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| {
            (d.as_secs() + EPOCH_DIFF) * 10_000_000 + u64::from(d.subsec_nanos() / 100)
        })
}

/// Folders are plain folders (on Windows the read-only bit of a folder means something
/// else); files of a read-only source are read-only.
fn attributes(a: &Attr) -> u32 {
    match (a.is_dir, a.readonly) {
        (true, _) => FILE_ATTRIBUTE_DIRECTORY,
        (false, false) => FILE_ATTRIBUTE_ARCHIVE,
        (false, true) => FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_READONLY,
    }
}

fn fill(info: &mut FileInfo, a: &Attr) {
    let t = filetime(a.modified);
    info.file_attributes = attributes(a);
    info.file_size = a.size;
    info.allocation_size = a.size.div_ceil(4096) * 4096;
    info.creation_time = t;
    info.last_access_time = t;
    info.last_write_time = t;
    info.change_time = t;
    info.index_number = 0;
    info.hard_links = 0;
}

pub(crate) struct Fs {
    fs: Arc<MountFs>,
    /// Self-relative security descriptor every file gets: the user, SYSTEM and
    /// Administrators.
    sd: Vec<u8>,
}

pub(crate) struct File {
    h: crate::Handle,
    is_dir: bool,
    dir: DirBuffer,
}

impl Fs {
    fn opened(&self, h: crate::Handle, info: &mut FileInfo) -> winfsp::Result<File> {
        let attr = self.fs.handle_attr(h).map_err(nt);
        let attr = match attr {
            Ok(a) => a,
            Err(e) => {
                let _ = self.fs.release(h);
                return Err(e);
            }
        };
        fill(info, &attr);
        Ok(File {
            h,
            is_dir: attr.is_dir,
            dir: DirBuffer::new(),
        })
    }

    fn info(&self, f: &File, info: &mut FileInfo) -> winfsp::Result<()> {
        fill(info, &self.fs.handle_attr(f.h).map_err(nt)?);
        Ok(())
    }
}

impl FileSystemContext for Fs {
    type FileContext = File;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let attr = self.fs.stat(&path(file_name)?).map_err(nt)?;
        if let Some(buf) = security_descriptor {
            if buf.len() >= self.sd.len() {
                // SAFETY: `buf` has room for `sd` (c_void is one byte).
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.sd.as_ptr(),
                        buf.as_mut_ptr().cast::<u8>(),
                        self.sd.len(),
                    )
                };
            }
        }
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: attributes(&attr),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<File> {
        let p = path(file_name)?;
        let attr = self.fs.stat(&p).map_err(nt)?;
        if create_options & FILE_DIRECTORY_FILE != 0 && !attr.is_dir {
            return Err(status(STATUS_NOT_A_DIRECTORY));
        }
        let write = !attr.is_dir && granted_access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        let (h, _) = self.fs.open(&p, write, false).map_err(nt)?;
        self.opened(h, file_info.as_mut())
    }

    fn close(&self, f: File) {
        if let Err(e) = self.fs.release(f.h) {
            tracing::warn!("mount: close: {e}");
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<File> {
        let p = path(file_name)?;
        let h = if create_options & FILE_DIRECTORY_FILE != 0 {
            self.fs.mkdir(&p).map_err(nt)?;
            self.fs.open(&p, false, false).map_err(nt)?.0
        } else {
            self.fs.create(&p).map_err(nt)?.0
        };
        self.opened(h, file_info.as_mut())
    }

    fn cleanup(&self, f: &File, _file_name: Option<&U16CStr>, flags: u32) {
        if flags & CLEANUP_DELETE == 0 {
            // CloseHandle: the only writer publishes now (Close may come much later).
            if let Err(e) = self.fs.flush(f.h) {
                tracing::warn!("mount: close: {e}");
            }
            return;
        }
        let result = self.fs.handle_path(f.h).and_then(|p| {
            if f.is_dir {
                self.fs.remove_dir(&p)
            } else {
                self.fs.remove_file(&p)
            }
        });
        if let Err(e) = result {
            tracing::warn!("mount: delete: {e}");
        }
    }

    fn flush(&self, f: Option<&File>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        match f {
            Some(f) => self.info(f, file_info),
            None => Ok(()),
        }
    }

    fn get_file_info(&self, f: &File, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.info(f, file_info)
    }

    fn overwrite(
        &self,
        f: &File,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.fs.set_len(f.h, 0).map_err(nt)?;
        self.info(f, file_info)
    }

    fn read_directory(
        &self,
        f: &File,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if let Ok(lock) = f.dir.acquire(marker.is_none(), None) {
            let p = self.fs.handle_path(f.h).map_err(nt)?;
            let mut entries = self.fs.list(&p).map_err(nt)?;
            if !p.is_root() {
                let dot = Attr {
                    is_dir: true,
                    size: 0,
                    modified: None,
                    readonly: false,
                };
                for name in [".", ".."] {
                    entries.push(crate::DirEntry {
                        name: name.into(),
                        attr: dot,
                    });
                }
            }
            for e in entries {
                let mut info: DirInfo = DirInfo::new();
                // Names longer than 255 UTF-16 units do not fit a Windows name: skipped.
                if info.set_name(&e.name).is_err() {
                    continue;
                }
                fill(info.file_info_mut(), &e.attr);
                lock.write(&mut info)?;
            }
        }
        Ok(f.dir.read(marker, buffer))
    }

    fn rename(
        &self,
        _f: &File,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        self.fs
            .rename(&path(file_name)?, &path(new_file_name)?, replace_if_exists)
            .map_err(nt)
    }

    fn set_basic_info(
        &self,
        f: &File,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // Times and attributes are the source's own; accepted and ignored.
        self.info(f, file_info)
    }

    fn set_delete(&self, f: &File, _file_name: &U16CStr, delete_file: bool) -> winfsp::Result<()> {
        if delete_file && f.is_dir {
            let p = self.fs.handle_path(f.h).map_err(nt)?;
            if !self.fs.list(&p).map_err(nt)?.is_empty() {
                return Err(status(STATUS_DIRECTORY_NOT_EMPTY));
            }
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        f: &File,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if !set_allocation_size {
            self.fs.set_len(f.h, new_size).map_err(nt)?;
        }
        self.info(f, file_info)
    }

    fn read(&self, f: &File, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        let n = self.fs.read(f.h, offset, buffer).map_err(nt)?;
        if n == 0 && !buffer.is_empty() {
            return Err(status(STATUS_END_OF_FILE));
        }
        Ok(n as u32)
    }

    fn write(
        &self,
        f: &File,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let size = self.fs.handle_attr(f.h).map_err(nt)?.size;
        let offset = if write_to_eof { size } else { offset };
        let data = if constrained_io {
            let room = size.saturating_sub(offset);
            &buffer[..buffer
                .len()
                .min(usize::try_from(room).unwrap_or(usize::MAX))]
        } else {
            buffer
        };
        let n = if data.is_empty() {
            0
        } else {
            self.fs.write(f.h, offset, data).map_err(nt)?
        };
        self.info(f, file_info)?;
        Ok(n as u32)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        // The source volume's space; unknown is a large, empty-looking volume, since
        // Explorer refuses to copy onto a drive that reports no free space.
        let (free, total) = self
            .fs
            .space()
            .map_or((1 << 40, 1 << 40), |s| (s.free, s.total));
        out.total_size = total;
        out.free_size = free;
        out.set_volume_label(self.fs.label());
        Ok(())
    }
}

/// What to say when WinFsp is not installed.
const NO_WINFSP: &str = "WinFsp is not installed: install it (https://winfsp.dev) to mount";

/// Why WinFsp cannot be loaded (not installed, or its DLL is gone), if it cannot.
pub(crate) fn driver_missing() -> Option<String> {
    match dll_path() {
        Some(dll) if std::path::Path::new(&dll).is_file() => None,
        _ => Some(NO_WINFSP.into()),
    }
}

/// `<WinFsp InstallDir>bin\winfsp-<arch>.dll`, from the registry.
fn dll_path() -> Option<String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 520];
    let mut size = (buf.len() * 2) as u32;
    let mut found = false;
    for key in [w!("SOFTWARE\\WOW6432Node\\WinFsp"), w!("SOFTWARE\\WinFsp")] {
        size = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `size` bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                w!("InstallDir"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if rc.is_ok() {
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    let mut dir = String::from_utf16_lossy(&buf[..len]);
    if !dir.ends_with('\\') {
        dir.push('\\');
    }
    let arch = match std::env::consts::ARCH {
        "x86" => "x86",
        "aarch64" => "a64",
        _ => "x64",
    };
    Some(format!("{dir}bin\\winfsp-{arch}.dll"))
}

/// Loads WinFsp's DLL ([`dll_path`]) so winfsp's delay-loaded imports resolve (the
/// installer does not put it on PATH).
fn load_dll() -> anyhow::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    let dll = HSTRING::from(dll_path().ok_or_else(|| anyhow::anyhow!(NO_WINFSP))?);
    // SAFETY: loads a library by full path; it stays loaded for the process.
    unsafe { LoadLibraryW(&dll) }.map_err(|e| anyhow::anyhow!("loading {dll}: {e}"))?;
    winfsp::winfsp_init().map_err(|e| anyhow::anyhow!("WinFsp: {e:?}"))?;
    Ok(())
}

/// The security descriptor of every file in the mount (self-relative bytes).
fn security_descriptor() -> anyhow::Result<Vec<u8>> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    let sid = keel_vfs::pipe::user_sid()?;
    let sddl = HSTRING::from(format!(
        "O:{sid}G:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)"
    ));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let mut len = 0u32;
    // SAFETY: on success `sd` points at `len` bytes we copy and then free.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut len),
        )?;
        let bytes = std::slice::from_raw_parts(sd.0.cast::<u8>(), len as usize).to_vec();
        let _ = LocalFree(HLOCAL(sd.0));
        Ok(bytes)
    }
}

pub(crate) fn mount(fs: Arc<MountFs>, target: &str) -> anyhow::Result<Box<dyn Send>> {
    load_dll()?;
    let mut params = VolumeParams::new();
    let created = filetime(Some(SystemTime::now()));
    params
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_creation_time(created)
        .volume_serial_number(created as u32)
        .file_info_timeout(1000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .post_cleanup_when_modified_only(true)
        // Cached data reaches the staged write before the handle closes.
        .flush_and_purge_on_cleanup(true)
        .filesystem_name("Keel");
    let sd = security_descriptor()?;
    let mut host = FileSystemHost::<Fs, FineGuard>::new(params, Fs { fs, sd })
        .map_err(|e| anyhow::anyhow!("WinFsp: {e:?}"))?;
    host.mount(target)
        .map_err(|e| anyhow::anyhow!("mounting at {target}: {e:?}"))?;
    host.start()
        .map_err(|e| anyhow::anyhow!("starting the WinFsp dispatcher: {e:?}"))?;
    // Dropping the host unmounts it and stops the dispatcher.
    Ok(Box::new(host))
}
