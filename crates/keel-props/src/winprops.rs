//! The Windows Property System: any canonical property (`System.Music.Artist`) of any file
//! through `IPropertyStore` (`SHGetPropertyStoreFromParsingName`), as the shell and Explorer
//! read it, with every property handler installed on the machine.

use crate::value::filetime_to_system_time;
use crate::{Error, Prop};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, IBindCtx, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, PSGetPropertyKeyFromName, SHGetPropertyStoreFromParsingName, GPS_BESTEFFORT,
    GPS_FASTPROPERTIESONLY, PROPERTYKEY,
};

/// COM for this call; uninitialized on drop when this call initialized it.
struct Com(bool);

impl Com {
    fn init() -> Com {
        // SAFETY: plain COM initialization for the calling thread; balanced in `drop`.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK and S_FALSE both need a matching CoUninitialize; RPC_E_CHANGED_MODE (a failure)
        // means the thread already has an apartment, which works for property stores too.
        Com(hr.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: matches the successful CoInitializeEx in `init`.
            unsafe { CoUninitialize() };
        }
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// The property key for a canonical name, or `None` when the system does not know it.
pub fn property_key(canonical: &str) -> Option<PROPERTYKEY> {
    let _com = Com::init();
    let name = wide(std::ffi::OsStr::new(canonical));
    let mut key = PROPERTYKEY::default();
    // SAFETY: `name` is NUL-terminated and outlives the call; `key` is a valid out pointer.
    unsafe { PSGetPropertyKeyFromName(PCWSTR(name.as_ptr()), &mut key) }.ok()?;
    Some(key)
}

/// Reads the canonical property `canonical` of `path`. With `fast_only`, only properties the
/// file system or index already has are read: no property handler opens the file, so a
/// cloud placeholder is never downloaded.
pub fn get(path: &Path, canonical: &str, fast_only: bool) -> Result<Prop, Error> {
    let _com = Com::init();
    let name = wide(std::ffi::OsStr::new(canonical));
    let mut key = PROPERTYKEY::default();
    // SAFETY: as in `property_key`.
    if unsafe { PSGetPropertyKeyFromName(PCWSTR(name.as_ptr()), &mut key) }.is_err() {
        return Err(Error::UnknownProperty(canonical.to_owned()));
    }
    let path_w = wide(path.as_os_str());
    let flags = if fast_only {
        GPS_FASTPROPERTIESONLY
    } else {
        GPS_BESTEFFORT
    };
    // SAFETY: `path_w` is NUL-terminated and outlives the call; no bind context.
    let store: IPropertyStore = match unsafe {
        SHGetPropertyStoreFromParsingName(PCWSTR(path_w.as_ptr()), None::<&IBindCtx>, flags)
    } {
        Ok(s) => s,
        Err(_) if !path.exists() => {
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )))
        }
        Err(_) => return Ok(Prop::Unknown),
    };
    // SAFETY: `key` is a valid PROPERTYKEY; the returned PROPVARIANT owns its data and is
    // cleared on drop.
    let Ok(value) = (unsafe { store.GetValue(&key) }) else {
        return Ok(Prop::Unknown);
    };
    Ok(convert(&value))
}

const VT_EMPTY: u16 = 0;
const VT_NULL: u16 = 1;
const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_R4: u16 = 4;
const VT_R8: u16 = 5;
const VT_BOOL: u16 = 11;
const VT_I1: u16 = 16;
const VT_UI1: u16 = 17;
const VT_UI2: u16 = 18;
const VT_UI4: u16 = 19;
const VT_I8: u16 = 20;
const VT_UI8: u16 = 21;
const VT_INT: u16 = 22;
const VT_UINT: u16 = 23;
const VT_FILETIME: u16 = 64;
const VT_VECTOR: u16 = 0x1000;

/// A PROPVARIANT as a [`Prop`]: numbers, booleans and dates by type; vectors as lists; any
/// other type through the system's own string conversion.
fn convert(v: &windows::core::PROPVARIANT) -> Prop {
    let raw = v.as_raw() as *const _ as *const u8;
    // SAFETY: a PROPVARIANT starts with its 16-bit VARTYPE; the value union follows the three
    // reserved words at offset 8 (the C layout, on every Windows target).
    let vt = unsafe { std::ptr::read_unaligned(raw.cast::<u16>()) };
    let string = || {
        windows::core::BSTR::try_from(v)
            .ok()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    match vt {
        VT_EMPTY | VT_NULL => Prop::Unknown,
        VT_BOOL => bool::try_from(v).map_or(Prop::Unknown, Prop::Bool),
        VT_I1 | VT_I2 | VT_I4 | VT_I8 | VT_INT => i64::try_from(v).map_or(Prop::Unknown, Prop::Int),
        VT_UI1 | VT_UI2 | VT_UI4 | VT_UINT | VT_UI8 => u64::try_from(v)
            .map_or(Prop::Unknown, |n| {
                Prop::Int(i64::try_from(n).unwrap_or(i64::MAX))
            }),
        VT_R4 | VT_R8 => f64::try_from(v).map_or(Prop::Unknown, Prop::Float),
        VT_FILETIME => {
            // SAFETY: for VT_FILETIME the union holds a FILETIME (two u32, low first).
            let ticks = unsafe { std::ptr::read_unaligned(raw.add(8).cast::<u64>()) };
            match filetime_to_system_time(ticks) {
                Some(t) if ticks != 0 => Prop::Date(t),
                _ => Prop::Unknown,
            }
        }
        t if t & VT_VECTOR != 0 => match string() {
            // The system joins vector elements with "; ".
            Some(s) => Prop::List(
                s.split("; ")
                    .filter(|p| !p.is_empty())
                    .map(|p| Prop::Str(p.to_owned()))
                    .collect(),
            ),
            None => Prop::Unknown,
        },
        _ => string().map_or(Prop::Unknown, Prop::Str),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_names() {
        assert!(property_key("System.Title").is_some());
        assert!(property_key("System.Music.Artist").is_some());
        assert!(property_key("System.NoSuchPropertyForKeel").is_none());
    }

    #[test]
    fn reads_a_core_property_of_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        std::fs::write(&p, b"hello").unwrap();
        // System.Size comes from the file system: present for every file.
        assert_eq!(get(&p, "System.Size", false).unwrap(), Prop::Int(5));
        assert_eq!(get(&p, "System.Size", true).unwrap(), Prop::Int(5));
        match get(&p, "System.ItemNameDisplay", false).unwrap() {
            Prop::Str(s) => assert!(s.starts_with("note"), "{s}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            get(&p, "System.DateModified", false).unwrap(),
            Prop::Date(_)
        ));
        assert!(matches!(
            get(&p, "System.NoSuchPropertyForKeel", false),
            Err(Error::UnknownProperty(_))
        ));
        assert!(matches!(
            get(&dir.path().join("missing.txt"), "System.Size", false),
            Err(Error::Io(_))
        ));
    }
}
