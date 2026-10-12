//! Portable devices (phones, cameras, media players) as scan roots: on Windows through
//! Windows Portable Devices (WPD, the COM API Explorer uses for MTP and PTP devices),
//! elsewhere none yet (libmtp or gphoto2 could come later).
//!
//! A device is opened **read-only** (`GENERIC_READ` client access) and only listed: no
//! object is created, changed, moved or deleted, and no file content is read. Each storage
//! of a device (internal storage, an SD card) is one scan root, [`super::Root::Mtp`].
//!
//! The walk itself (filters, depth, cancel, the time budget) is in `walk.rs`; this module
//! is the device side: the [`Backend`] and [`Session`] traits (WPD, or a mock in the
//! tests), and how WPD's object properties map to tree items ([`RawObject::entry`]).

use super::{attr, Space, UNKNOWN_TIME};
use anyhow::Result;

/// A connected portable device and its storages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The PnP device id (opaque; stable while the device stays plugged into the same port).
    pub id: String,
    /// The device's friendly name ("Pixel 7").
    pub name: String,
    pub storages: Vec<Storage>,
}

/// One storage of a device: what a scan root of a device is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Storage {
    /// The storage's object id on the device (opaque).
    pub id: String,
    /// Its description ("Internal shared storage", "SD card").
    pub name: String,
    /// Capacity and free space when the device states them.
    pub space: Option<Space>,
}

/// What a device says an object is (`WPD_OBJECT_CONTENT_TYPE`).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Folder,
    /// A functional object (a storage, or another device function): a container.
    Functional,
    /// Any other content type: a file (image, audio, document, generic file...).
    File,
    /// No content type given.
    Unknown,
}

/// A date as a device gives it.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WpdTime {
    /// `VT_DATE`: days since 1899-12-30 (taken as UTC; devices rarely state a zone).
    Ole(f64),
    /// `VT_FILETIME`: 100 ns ticks since 1601-01-01 UTC.
    FileTime(u64),
}

impl WpdTime {
    /// Unix nanoseconds, [`UNKNOWN_TIME`] for a value out of range (or the OLE zero date,
    /// which devices use for "not set").
    pub(crate) fn unix_ns(self) -> i64 {
        const DAY_NS: f64 = 86_400e9;
        // 1899-12-30 to 1970-01-01.
        const OLE_EPOCH_DAYS: f64 = 25_569.0;
        // 1601-01-01 to 1970-01-01 in 100 ns ticks.
        const FT_EPOCH: u64 = 116_444_736_000_000_000;
        match self {
            WpdTime::Ole(d) => {
                if !d.is_finite() || d <= 0.0 {
                    return UNKNOWN_TIME;
                }
                let ns = (d - OLE_EPOCH_DAYS) * DAY_NS;
                // Years 1900 to 2200.
                if !(-2.3e18..=7.3e18).contains(&ns) {
                    return UNKNOWN_TIME;
                }
                ns as i64
            }
            WpdTime::FileTime(0) => UNKNOWN_TIME,
            WpdTime::FileTime(t) => {
                let ticks = i128::from(t) - i128::from(FT_EPOCH);
                i64::try_from(ticks * 100).unwrap_or(UNKNOWN_TIME)
            }
        }
    }
}

/// An object below a storage, with the properties a scan reads, as the device gives them.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawObject {
    /// The object id (opaque, unique on the device).
    pub id: String,
    /// `WPD_OBJECT_NAME`.
    pub name: Option<String>,
    /// `WPD_OBJECT_ORIGINAL_FILE_NAME` (the file name with its extension).
    pub original_name: Option<String>,
    pub kind: ContentKind,
    /// `WPD_OBJECT_SIZE`.
    pub size: Option<u64>,
    pub modified: Option<WpdTime>,
    pub created: Option<WpdTime>,
    pub hidden: bool,
    pub system: bool,
}

/// [`RawObject`] mapped to what a tree item needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub folder: bool,
    pub size: u64,
    /// Unix ns or [`UNKNOWN_TIME`].
    pub modified: i64,
    /// [`attr`] bits.
    pub attrs: u32,
}

impl RawObject {
    /// A folder or a file: the original file name when there is one (it has the
    /// extension), else the name, else the object id; separators and control characters
    /// replaced so the item has a path of its own; the last write time, else the creation
    /// time; hidden and system attributes.
    pub(crate) fn entry(&self) -> Entry {
        let pick = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let raw = pick(&self.original_name)
            .or_else(|| pick(&self.name))
            .unwrap_or(&self.id);
        let folder = match self.kind {
            ContentKind::Folder | ContentKind::Functional => true,
            ContentKind::File => false,
            // No content type: an object without a size is taken for a folder.
            ContentKind::Unknown => self.size.is_none(),
        };
        let modified = (self.modified.map(WpdTime::unix_ns))
            .filter(|&t| t != UNKNOWN_TIME)
            .or_else(|| self.created.map(WpdTime::unix_ns))
            .unwrap_or(UNKNOWN_TIME);
        let mut attrs = 0;
        if folder {
            attrs |= attr::DIRECTORY;
        }
        if self.hidden {
            attrs |= attr::HIDDEN;
        }
        if self.system {
            attrs |= attr::SYSTEM;
        }
        Entry {
            name: clean_name(raw),
            folder,
            size: if folder { 0 } else { self.size.unwrap_or(0) },
            modified,
            attrs,
        }
    }
}

/// A device's name as a path part: `/`, `\`, NUL and control characters become `_`, as do
/// the names `.` and `..`; at most 255 characters.
pub(crate) fn clean_name(s: &str) -> String {
    let out: String = (s.chars())
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(255)
        .collect();
    match out.as_str() {
        "" | "." | ".." => "_".repeat(out.len().max(1)),
        _ => out,
    }
}

/// Lists portable devices and opens them. All calls of one scan happen on one thread
/// ([`on_device_thread`]); a backend is shared between threads only to be handed there.
pub(crate) trait Backend: Sync {
    /// The connected devices and their storages.
    fn devices(&self) -> Result<Vec<Device>>;
    /// Opens a device (read-only) for listing.
    fn open<'a>(&'a self, device_id: &str) -> Result<Box<dyn Session + 'a>>;
}

/// An open device.
pub(crate) trait Session {
    /// The device's friendly name.
    fn name(&mut self) -> String;
    /// One storage's description and space.
    fn storage(&mut self, storage_id: &str) -> Result<Storage>;
    /// The objects directly below `parent` (a storage or a folder). `stop` is asked
    /// between objects; when it says stop, what was read so far is returned (the caller
    /// discards it).
    fn children(&mut self, parent: &str, stop: &dyn Fn() -> bool) -> Result<Vec<RawObject>>;
}

/// The system's devices: Windows Portable Devices on Windows, none elsewhere.
pub(crate) struct System;

impl Backend for System {
    fn devices(&self) -> Result<Vec<Device>> {
        #[cfg(windows)]
        {
            wpd::devices()
        }
        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    fn open<'a>(&'a self, device_id: &str) -> Result<Box<dyn Session + 'a>> {
        #[cfg(windows)]
        {
            Ok(Box::new(wpd::WpdSession::open(device_id, None)?))
        }
        #[cfg(not(windows))]
        {
            let _ = device_id;
            anyhow::bail!("portable devices can be scanned on Windows only")
        }
    }
}

/// Runs `f` on a new thread set up for COM (the multithreaded apartment on Windows), so
/// device calls never depend on the caller's apartment (the UI thread's is single
/// threaded). Every COM object `f` makes must be dropped inside it.
pub(crate) fn on_device_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        let worker = std::thread::Builder::new()
            .name("keel-du-device".into())
            .spawn_scoped(s, || {
                #[cfg(windows)]
                let _com = wpd::Com::init();
                f()
            })
            .expect("device thread");
        match worker.join() {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// The connected portable devices with their storages (Windows; none elsewhere). Talks to
/// every device: call it off the UI thread.
pub fn devices() -> Result<Vec<Device>> {
    on_device_thread(|| System.devices())
}

#[cfg(windows)]
pub(crate) mod wpd {
    //! The Windows Portable Devices calls: the device manager's list, a device opened with
    //! read access only, object enumeration and property reads. Nothing here writes.

    use super::{ContentKind, Device, RawObject, Session, Storage, WpdTime};
    use crate::du::Space;
    use anyhow::{Context, Result};
    use windows::core::{w, GUID, PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::{
        IPortableDevice, IPortableDeviceContent, IPortableDeviceKeyCollection,
        IPortableDeviceManager, IPortableDeviceProperties, IPortableDeviceValues,
        PortableDeviceFTM, PortableDeviceKeyCollection, PortableDeviceManager,
        PortableDeviceValues, WPD_CLIENT_DESIRED_ACCESS, WPD_CLIENT_MAJOR_VERSION,
        WPD_CLIENT_MINOR_VERSION, WPD_CLIENT_NAME, WPD_CLIENT_REVISION,
        WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE, WPD_CONTENT_TYPE_FOLDER,
        WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_DEVICE_OBJECT_ID, WPD_FUNCTIONAL_CATEGORY_STORAGE,
        WPD_FUNCTIONAL_OBJECT_CATEGORY, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_DATE_CREATED,
        WPD_OBJECT_DATE_MODIFIED, WPD_OBJECT_ISHIDDEN, WPD_OBJECT_ISSYSTEM, WPD_OBJECT_NAME,
        WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE, WPD_STORAGE_CAPACITY,
        WPD_STORAGE_DESCRIPTION, WPD_STORAGE_FREE_SPACE_IN_BYTES,
    };
    use windows::Win32::Foundation::S_OK;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    /// `GENERIC_READ`: the device is opened for reading only.
    const GENERIC_READ: u32 = 0x8000_0000;
    /// `SECURITY_IMPERSONATION` (`SecurityImpersonation << 16`), as Microsoft's samples.
    const SECURITY_IMPERSONATION: u32 = 0x0002_0000;
    const VT_DATE: u16 = 7;
    const VT_FILETIME: u16 = 64;
    /// Object ids fetched per enumeration call.
    const BATCH: usize = 64;

    /// COM on the current thread for as long as it lives.
    pub(crate) struct Com(bool);

    impl Com {
        pub(crate) fn init() -> Self {
            // SAFETY: plain COM initialization; undone in Drop only when it succeeded.
            Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balanced with the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// A string WPD allocated: copied, then freed.
    ///
    /// # Safety
    /// `p` is null or a NUL-terminated string from `CoTaskMemAlloc`, not used afterwards.
    unsafe fn take(p: PWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }

    fn manager() -> Result<IPortableDeviceManager> {
        // SAFETY: creating an in-process COM object.
        unsafe { CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER) }
            .context("Windows Portable Devices is not available")
    }

    /// The PnP ids of the connected devices.
    fn device_ids(mgr: &IPortableDeviceManager) -> Result<Vec<String>> {
        // SAFETY: the first call only counts; the second fills `ids` (of that length);
        // every string it returns is freed by `take`.
        unsafe {
            let _ = mgr.RefreshDeviceList();
            let mut n = 0u32;
            mgr.GetDevices(std::ptr::null_mut(), &mut n)
                .context("cannot list portable devices")?;
            if n == 0 {
                return Ok(Vec::new());
            }
            let mut ids = vec![PWSTR::null(); n as usize];
            let got = mgr.GetDevices(ids.as_mut_ptr(), &mut n);
            let mut out = Vec::with_capacity(ids.len());
            for p in ids {
                let s = take(p);
                if !s.is_empty() {
                    out.push(s);
                }
            }
            got.context("cannot list portable devices")?;
            Ok(out)
        }
    }

    fn friendly_name(mgr: &IPortableDeviceManager, id: &str) -> Option<String> {
        let w = wide(id);
        let mut len = 0u32;
        // SAFETY: the first call asks for the length; the buffer has room for it.
        unsafe {
            mgr.GetDeviceFriendlyName(PCWSTR(w.as_ptr()), PWSTR::null(), &mut len)
                .ok()?;
            let mut buf = vec![0u16; len as usize + 1];
            len = buf.len() as u32;
            mgr.GetDeviceFriendlyName(PCWSTR(w.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut len)
                .ok()?;
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let s = String::from_utf16_lossy(&buf[..end]);
            (!s.trim().is_empty()).then_some(s)
        }
    }

    pub(crate) fn devices() -> Result<Vec<Device>> {
        let mgr = manager()?;
        let mut out = Vec::new();
        for id in device_ids(&mgr)? {
            let name = friendly_name(&mgr, &id);
            let mut s = match WpdSession::open(&id, name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("du: portable device not opened: {e:#}");
                    continue;
                }
            };
            let storages = s.storages().unwrap_or_else(|e| {
                tracing::debug!("du: portable device storages: {e:#}");
                Vec::new()
            });
            out.push(Device {
                name: s.name(),
                id,
                storages,
            });
        }
        Ok(out)
    }

    fn keys(list: &[PROPERTYKEY]) -> Result<IPortableDeviceKeyCollection> {
        // SAFETY: an in-process COM object; the keys are copied in.
        unsafe {
            let k: IPortableDeviceKeyCollection =
                CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER)?;
            for key in list {
                k.Add(key)?;
            }
            Ok(k)
        }
    }

    fn string(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<String> {
        // SAFETY: the returned string is WPD's to free, done by `take`.
        let s = unsafe { take(v.GetStringValue(key).ok()?) };
        (!s.is_empty()).then_some(s)
    }

    fn time(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<WpdTime> {
        // SAFETY: the union member read is the one `vt` names.
        unsafe {
            let pv = v.GetValue(key).ok()?;
            let raw = pv.as_raw();
            let inner = &raw.Anonymous.Anonymous;
            match inner.vt {
                VT_DATE => Some(WpdTime::Ole(inner.Anonymous.date)),
                VT_FILETIME => {
                    let ft = inner.Anonymous.filetime;
                    Some(WpdTime::FileTime(
                        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime),
                    ))
                }
                _ => None,
            }
        }
    }

    fn flag(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> bool {
        // SAFETY: a plain property read.
        unsafe { v.GetBoolValue(key) }.is_ok_and(|b| b.as_bool())
    }

    fn guid(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<GUID> {
        // SAFETY: a plain property read.
        unsafe { v.GetGuidValue(key) }.ok()
    }

    fn number(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<u64> {
        // SAFETY: a plain property read.
        unsafe { v.GetUnsignedLargeIntegerValue(key) }.ok()
    }

    /// A device opened for reading.
    pub(crate) struct WpdSession {
        device: IPortableDevice,
        content: IPortableDeviceContent,
        props: IPortableDeviceProperties,
        object_keys: IPortableDeviceKeyCollection,
        storage_keys: IPortableDeviceKeyCollection,
        name: Option<String>,
    }

    impl WpdSession {
        /// Opens the device `id` with read access only (`name`: its friendly name when
        /// known).
        pub(crate) fn open(id: &str, name: Option<String>) -> Result<Self> {
            let name = name.or_else(|| manager().ok().and_then(|m| friendly_name(&m, id)));
            let w = wide(id);
            // SAFETY: in-process COM objects; `w` outlives the Open call.
            unsafe {
                let info: IPortableDeviceValues =
                    CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
                info.SetStringValue(&WPD_CLIENT_NAME, w!("Keel"))?;
                info.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 1)?;
                info.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 0)?;
                info.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0)?;
                info.SetUnsignedIntegerValue(
                    &WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE,
                    SECURITY_IMPERSONATION,
                )?;
                // Read only: Keel never writes to a device.
                info.SetUnsignedIntegerValue(&WPD_CLIENT_DESIRED_ACCESS, GENERIC_READ)?;
                let device: IPortableDevice =
                    CoCreateInstance(&PortableDeviceFTM, None, CLSCTX_INPROC_SERVER)?;
                device.Open(PCWSTR(w.as_ptr()), &info).map_err(|e| {
                    let hint = if e.code().0 as u32 == 0x8007_0005 {
                        " (unlock the device and allow file access on it)"
                    } else {
                        ""
                    };
                    anyhow::anyhow!("cannot open the portable device{hint}: {e}")
                })?;
                let content = device.Content()?;
                let props = content.Properties()?;
                Ok(WpdSession {
                    object_keys: keys(&[
                        WPD_OBJECT_NAME,
                        WPD_OBJECT_ORIGINAL_FILE_NAME,
                        WPD_OBJECT_CONTENT_TYPE,
                        WPD_OBJECT_SIZE,
                        WPD_OBJECT_DATE_MODIFIED,
                        WPD_OBJECT_DATE_CREATED,
                        WPD_OBJECT_ISHIDDEN,
                        WPD_OBJECT_ISSYSTEM,
                    ])?,
                    storage_keys: keys(&[
                        WPD_OBJECT_NAME,
                        WPD_OBJECT_CONTENT_TYPE,
                        WPD_FUNCTIONAL_OBJECT_CATEGORY,
                        WPD_STORAGE_DESCRIPTION,
                        WPD_STORAGE_CAPACITY,
                        WPD_STORAGE_FREE_SPACE_IN_BYTES,
                    ])?,
                    device,
                    content,
                    props,
                    name,
                })
            }
        }

        fn values(
            &self,
            id: &str,
            keys: &IPortableDeviceKeyCollection,
        ) -> Result<IPortableDeviceValues> {
            let w = wide(id);
            // SAFETY: `w` outlives the call.
            Ok(unsafe { self.props.GetValues(PCWSTR(w.as_ptr()), keys) }?)
        }

        /// Object ids directly below `parent`.
        fn ids(&self, parent: &str, stop: &dyn Fn() -> bool) -> Result<Vec<String>> {
            let w = wide(parent);
            let mut out = Vec::new();
            // SAFETY: `w` outlives the call; every id returned is freed by `take`.
            unsafe {
                let en = self.content.EnumObjects(0, PCWSTR(w.as_ptr()), None)?;
                while !stop() {
                    let mut batch = [PWSTR::null(); BATCH];
                    let mut got = 0u32;
                    let hr = en.Next(&mut batch, &mut got);
                    for p in batch.into_iter().take((got as usize).min(BATCH)) {
                        out.push(take(p));
                    }
                    hr.ok()?;
                    if hr != S_OK || got == 0 {
                        break;
                    }
                }
            }
            Ok(out)
        }

        /// The device's storages (functional objects of the storage category).
        pub(crate) fn storages(&mut self) -> Result<Vec<Storage>> {
            // SAFETY: WPD_DEVICE_OBJECT_ID is a static NUL-terminated string.
            let device = unsafe { WPD_DEVICE_OBJECT_ID.to_string() }?;
            let mut out = Vec::new();
            for id in self.ids(&device, &|| false)? {
                let Ok(v) = self.values(&id, &self.storage_keys) else {
                    continue;
                };
                if guid(&v, &WPD_FUNCTIONAL_OBJECT_CATEGORY)
                    == Some(WPD_FUNCTIONAL_CATEGORY_STORAGE)
                {
                    out.push(self.storage_of(id, &v));
                }
            }
            Ok(out)
        }

        fn storage_of(&self, id: String, v: &IPortableDeviceValues) -> Storage {
            let space = match (
                number(v, &WPD_STORAGE_CAPACITY),
                number(v, &WPD_STORAGE_FREE_SPACE_IN_BYTES),
            ) {
                (Some(total), Some(free)) if total > 0 => Some(Space {
                    total,
                    free: free.min(total),
                }),
                _ => None,
            };
            Storage {
                name: string(v, &WPD_STORAGE_DESCRIPTION)
                    .or_else(|| string(v, &WPD_OBJECT_NAME))
                    .unwrap_or_else(|| "Storage".into()),
                id,
                space,
            }
        }
    }

    impl Drop for WpdSession {
        fn drop(&mut self) {
            // SAFETY: closing the connection this session opened.
            unsafe {
                let _ = self.device.Close();
            }
        }
    }

    impl Session for WpdSession {
        fn name(&mut self) -> String {
            self.name
                .clone()
                .unwrap_or_else(|| "Portable device".into())
        }

        fn storage(&mut self, storage_id: &str) -> Result<Storage> {
            let v = self
                .values(storage_id, &self.storage_keys)
                .context("the storage is not on the device")?;
            Ok(self.storage_of(storage_id.to_owned(), &v))
        }

        fn children(&mut self, parent: &str, stop: &dyn Fn() -> bool) -> Result<Vec<RawObject>> {
            let ids = self.ids(parent, stop)?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if stop() {
                    break;
                }
                let v = match self.values(&id, &self.object_keys) {
                    Ok(v) => v,
                    Err(e) => {
                        // Gone since it was listed, or unreadable: left out.
                        tracing::debug!("du: device object {id}: {e:#}");
                        continue;
                    }
                };
                let kind = match guid(&v, &WPD_OBJECT_CONTENT_TYPE) {
                    Some(g) if g == WPD_CONTENT_TYPE_FOLDER => ContentKind::Folder,
                    Some(g) if g == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT => ContentKind::Functional,
                    Some(_) => ContentKind::File,
                    None => ContentKind::Unknown,
                };
                out.push(RawObject {
                    name: string(&v, &WPD_OBJECT_NAME),
                    original_name: string(&v, &WPD_OBJECT_ORIGINAL_FILE_NAME),
                    kind,
                    size: number(&v, &WPD_OBJECT_SIZE),
                    modified: time(&v, &WPD_OBJECT_DATE_MODIFIED),
                    created: time(&v, &WPD_OBJECT_DATE_CREATED),
                    hidden: flag(&v, &WPD_OBJECT_ISHIDDEN),
                    system: flag(&v, &WPD_OBJECT_ISSYSTEM),
                    id,
                });
            }
            Ok(out)
        }
    }
}
