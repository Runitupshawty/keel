//! Windows drag-out: `IDataObject` (`CF_HDROP` + `Preferred DropEffect`) and `IDropSource`
//! for `DoDragDrop` on a dedicated STA thread.
//!
//! The drag thread attaches its input state to the UI thread's (`AttachThreadInput`): the
//! mouse button went down in the UI thread's window, and without a shared input state the
//! drag thread's capture would never see the moves and the release.

use super::DropEffect;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::core::{implement, HRESULT};
use windows::Win32::Foundation::{
    GlobalFree, BOOL, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
    DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_TYMED, E_INVALIDARG, E_NOTIMPL, HGLOBAL,
    OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET,
    DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MK_RBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Shell::{SHCreateStdEnumFmtEtc, CFSTR_PREFERREDDROPEFFECT, DROPFILES};

/// A `DROPFILES` block: the 20-byte header (`pFiles` = 20, `fWide` = 1), then each path as
/// UTF-16 with its NUL, then one more NUL. Plain absolute paths: Explorer does not take
/// `\\?\` names here.
pub fn dropfiles(paths: &[PathBuf]) -> Result<Vec<u8>> {
    let header = std::mem::size_of::<DROPFILES>();
    let mut bytes = vec![0u8; header];
    bytes[..4].copy_from_slice(&(header as u32).to_le_bytes()); // pFiles
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        let units: Vec<u16> = p.as_os_str().encode_wide().collect();
        anyhow::ensure!(!units.contains(&0), "path contains NUL");
        for u in units.into_iter().chain([0]) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    Ok(bytes)
}

/// Starts an OS drag of `paths` from the UI thread (call it there: its thread id is what
/// the drag thread attaches to) and returns at once; `done` runs on the drag thread with
/// what the target did. `allow_move` lets the target move the files.
pub fn start(
    paths: Vec<PathBuf>,
    allow_move: bool,
    done: impl FnOnce(Result<DropEffect>) + Send + 'static,
) {
    let ui = unsafe { GetCurrentThreadId() };
    let spawned = std::thread::Builder::new()
        .name("keel-drag-out".into())
        .spawn(move || done(run(&paths, allow_move, Some(ui))));
    if let Err(e) = spawned {
        tracing::error!("spawn keel-drag-out: {e}");
    }
}

/// Runs the drag on the calling thread (made an OLE STA here) until the user drops or
/// cancels. `ui_thread`: the thread whose window the mouse button went down in.
pub fn run(paths: &[PathBuf], allow_move: bool, ui_thread: Option<u32>) -> Result<DropEffect> {
    let data = DataObject::new(paths)?;
    unsafe { OleInitialize(None) }.context("OleInitialize")?;
    // Declared first, dropped last: the COM objects below go before OLE is torn down.
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe { OleUninitialize() };
        }
    }
    let _ole = Ole;
    let data: IDataObject = data.into();
    let source: IDropSource = DropSource.into();
    let me = unsafe { GetCurrentThreadId() };
    let attached = ui_thread
        .filter(|&ui| ui != me)
        .filter(|&ui| unsafe { AttachThreadInput(me, ui, true) }.as_bool());
    let ok = if allow_move {
        DROPEFFECT_COPY | DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    let mut effect = DROPEFFECT_NONE;
    let hr = unsafe { DoDragDrop(&data, &source, ok, &mut effect) };
    if let Some(ui) = attached {
        let _ = unsafe { AttachThreadInput(me, ui, false) };
    }
    match hr {
        DRAGDROP_S_DROP if effect.0 & DROPEFFECT_MOVE.0 != 0 => Ok(DropEffect::Move),
        DRAGDROP_S_DROP if effect.0 & DROPEFFECT_COPY.0 != 0 => Ok(DropEffect::Copy),
        DRAGDROP_S_DROP | DRAGDROP_S_CANCEL => Ok(DropEffect::None),
        hr => Err(windows::core::Error::from(hr)).context("DoDragDrop"),
    }
}

fn preferred_effect_format() -> u16 {
    // Registered formats are 0xC000..=0xFFFF, so they fit a u16.
    unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16 }
}

/// A movable global block holding `bytes` (owned by whoever receives it).
fn hglobal(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let ptr = GlobalLock(mem) as *mut u8;
        if ptr.is_null() {
            let _ = GlobalFree(mem);
            return Err(windows::core::Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

#[implement(IDataObject)]
struct DataObject {
    hdrop: Vec<u8>,
    preferred: u16,
}

impl DataObject {
    fn new(paths: &[PathBuf]) -> Result<Self> {
        Ok(Self {
            hdrop: dropfiles(paths)?,
            preferred: preferred_effect_format(),
        })
    }

    fn formats(&self) -> [FORMATETC; 2] {
        let format = |cf: u16| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        [format(CF_HDROP.0), format(self.preferred)]
    }

    /// The bytes served for `f`, or why not.
    fn bytes_for(&self, f: *const FORMATETC) -> Result<Vec<u8>, HRESULT> {
        let f = unsafe { f.as_ref() }.ok_or(E_INVALIDARG)?;
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_FORMATETC);
        }
        let bytes = if f.cfFormat == CF_HDROP.0 {
            self.hdrop.clone()
        } else if f.cfFormat == self.preferred {
            // Copy by default; the target still moves on Shift (or by its own rules).
            DROPEFFECT_COPY.0.to_le_bytes().to_vec()
        } else {
            return Err(DV_E_FORMATETC);
        };
        if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED);
        }
        Ok(bytes)
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, f: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let bytes = self.bytes_for(f)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: hglobal(&bytes)?,
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        match self.bytes_for(f) {
            Ok(_) => S_OK,
            Err(hr) => hr,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        // Drag-image and drop-description formats the shell offers are optional.
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Option<&IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & (MK_LBUTTON.0 | MK_RBUTTON.0) == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Ole::ReleaseStgMedium;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    fn wide(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn dropfiles_layout() {
        let paths = [PathBuf::from(r"C:\a.txt"), PathBuf::from(r"D:\été\日本.md")];
        let bytes = dropfiles(&paths).unwrap();
        let mut want = vec![0u8; 20];
        want[0] = 20; // pFiles: the list starts right after the header
        want[16] = 1; // fWide
        want.extend(wide("C:\\a.txt\0D:\\été\\日本.md\0\0"));
        assert_eq!(bytes, want);
        // No paths: header plus the terminating NUL.
        assert_eq!(dropfiles(&[]).unwrap().len(), 22);
        assert!(dropfiles(&[PathBuf::from(OsString::from_wide(&[b'a' as u16, 0]))]).is_err());
    }

    #[test]
    fn data_object_serves_hdrop_and_preferred_effect() {
        let paths = vec![PathBuf::from(r"C:\x\one.txt"), PathBuf::from(r"C:\ü.bin")];
        let data: IDataObject = DataObject::new(&paths).unwrap().into();
        let [hdrop, preferred] = DataObject::new(&paths).unwrap().formats();
        unsafe {
            assert_eq!(data.QueryGetData(&hdrop), S_OK);
            let mut text = hdrop;
            text.cfFormat = 1; // CF_TEXT
            assert_eq!(data.QueryGetData(&text), DV_E_FORMATETC);

            let mut medium = data.GetData(&hdrop).unwrap();
            let drop = HDROP(medium.u.hGlobal.0);
            assert_eq!(DragQueryFileW(drop, u32::MAX, None), 2);
            let mut got = Vec::new();
            for i in 0..2 {
                let mut buf = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                let n = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                got.push(PathBuf::from(OsString::from_wide(&buf[..n])));
            }
            assert_eq!(got, paths);
            ReleaseStgMedium(&mut medium);

            let mut medium = data.GetData(&preferred).unwrap();
            let mem = medium.u.hGlobal;
            let effect = *(GlobalLock(mem) as *const u32);
            let _ = GlobalUnlock(mem);
            assert_eq!(effect, DROPEFFECT_COPY.0);
            ReleaseStgMedium(&mut medium);
        }
    }

    /// The real `DoDragDrop` on its own thread, cancelled by an Esc key message (posted to
    /// the drag thread, as the keyboard would), so nothing is dropped anywhere.
    #[test]
    fn escape_cancels_the_drag() {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_KEYDOWN};
        let (id_tx, id_rx) = crossbeam_channel::bounded(1);
        let drag = std::thread::spawn(move || {
            id_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            run(&[std::env::temp_dir()], true, None)
        });
        let id = id_rx.recv().unwrap();
        // Until the drag loop is up and sees it.
        for _ in 0..50 {
            if drag.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            let esc = WPARAM(0x1B); // VK_ESCAPE
            let _ = unsafe { PostThreadMessageW(id, WM_KEYDOWN, esc, LPARAM(0)) };
        }
        assert!(drag.is_finished(), "DoDragDrop did not end on Esc");
        assert_eq!(drag.join().unwrap().unwrap(), DropEffect::None);
    }
}
